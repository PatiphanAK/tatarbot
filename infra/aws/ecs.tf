# Two independent ECS services on one cluster: tatarbot-discord and tatarbot-runtime.
#
# Terraform owns the services and the *initial* task definitions. The deploy workflows
# register new task-definition revisions with new image tags, so the services ignore
# task_definition drift. A Terraform change to a task definition therefore takes effect
# on the next deploy, which copies the newest revision and swaps in the image.
#
# launch_type = "EC2" (default): both tasks on the one container instance from ec2.tf,
# bridge networking. The runtime publishes its port on the instance; the bot calls it over
# the Docker bridge gateway. Containers can't reach the instance metadata service
# (IMDSv2, hop limit 1), so they never see the instance role.
# launch_type = "FARGATE": one task each (awsvpc), bot -> runtime over Cloud Map DNS.

locals {
  runtime_url = (local.ec2
    ? "http://${var.ec2_docker_bridge_ip}:${var.runtime_port}"
    : "http://runtime.${local.namespace}:${var.runtime_port}"
  )

  # ECS instance attribute value for the placement constraint on EC2.
  ecs_cpu_architecture = var.cpu_architecture == "ARM64" ? "arm64" : "x86_64"

  # Placeholder for the first apply only; services start at desired count 0 (see README).
  bootstrap_tag = "bootstrap"

  log_options = {
    for k, _ in local.services : k => {
      logDriver = "awslogs"
      options = {
        awslogs-group         = aws_cloudwatch_log_group.service[k].name
        awslogs-region        = var.region
        awslogs-stream-prefix = k
      }
    }
  }

  # Restart a crashed container in place (no new task, no image pull). ECS-initiated
  # stops (deploys, scale-in) are not restarted.
  restart_policy = { enabled = true, restartAttemptPeriod = 60 }

  # On EC2, containers share the instance: a hard limit each, and a smaller reservation
  # that placement counts. On Fargate the task size sets both.
  container_memory = {
    runtime = { for k, v in { memory = var.runtime_memory, memoryReservation = var.runtime_memory_reservation } : k => v if local.ec2 }
    discord = { for k, v in { memory = var.discord_memory, memoryReservation = var.discord_memory_reservation } : k => v if local.ec2 }
  }
}

resource "aws_ecs_cluster" "main" {
  name = var.project

  setting {
    name  = "containerInsights"
    value = "disabled" # extra CloudWatch cost; enable when needed
  }
}

resource "aws_ecs_cluster_capacity_providers" "main" {
  cluster_name       = aws_ecs_cluster.main.name
  capacity_providers = ["FARGATE"]

  default_capacity_provider_strategy {
    capacity_provider = "FARGATE"
    weight            = 1
  }
}

resource "aws_cloudwatch_log_group" "service" {
  for_each = local.services

  name              = "/ecs/${each.value}"
  retention_in_days = var.log_retention_days
}

# ---- Agent Runtime

resource "aws_ecs_task_definition" "runtime" {
  family                   = local.services.runtime
  requires_compatibilities = [var.launch_type]
  network_mode             = local.ec2 ? "bridge" : "awsvpc"
  cpu                      = local.ec2 ? null : var.runtime_cpu
  memory                   = local.ec2 ? null : var.runtime_memory
  execution_role_arn       = aws_iam_role.execution["runtime"].arn

  dynamic "runtime_platform" {
    for_each = local.fargate ? [1] : []
    content {
      operating_system_family = "LINUX"
      cpu_architecture        = var.cpu_architecture
    }
  }

  # A wrong-architecture instance leaves the task pending instead of failing with
  # "exec format error".
  dynamic "placement_constraints" {
    for_each = local.ec2 ? [1] : []
    content {
      type       = "memberOf"
      expression = "attribute:ecs.cpu-architecture == ${local.ecs_cpu_architecture}"
    }
  }

  container_definitions = jsonencode([
    merge(local.container_memory.runtime, {
      name                   = "runtime"
      image                  = "${aws_ecr_repository.service["runtime"].repository_url}:${local.bootstrap_tag}"
      essential              = true
      readonlyRootFilesystem = true
      # On EC2 (bridge) the port is published on the instance, at the same number, so
      # only one runtime task fits; the security group admits nothing from outside.
      portMappings = [{ containerPort = var.runtime_port, hostPort = var.runtime_port, protocol = "tcp", name = "http" }]
      environment = [
        for k, v in merge(var.runtime_extra_environment, {
          UVICORN_HOST          = "0.0.0.0"
          UVICORN_PORT          = tostring(var.runtime_port)
          MODEL_BASE_URL        = var.model_base_url
          MODEL_NAME            = var.model_name
          MODEL_TIMEOUT_SECONDS = tostring(var.model_timeout_seconds)
          # Whole turn incl. web searches; leaves the bot time to report a timeout.
          TURN_TIMEOUT_SECONDS = tostring(var.agent_timeout_secs - 10)
        }) : { name = k, value = v }
      ]
      secrets = concat(
        [
          { name = "AGENT_API_KEY", valueFrom = local.secret_arns.agent_api_key },
          { name = "MODEL_API_KEY", valueFrom = local.secret_arns.model_api_key },
        ],
        # The runtime enables web search only when this key is set.
        var.web_search_enabled ? [
          { name = "TAVILY_API_KEY", valueFrom = local.secret_arns.tavily_api_key },
        ] : [],
      )
      # Liveness only (/health). /ready calls the inference backend and could wake or bill
      # a scale-to-zero endpoint on every probe.
      healthCheck = {
        command     = ["CMD", "python", "-c", "import os, urllib.request; urllib.request.urlopen('http://127.0.0.1:' + os.environ['UVICORN_PORT'] + '/health', timeout=3)"]
        interval    = 30
        timeout     = 5
        retries     = 3
        startPeriod = 15
      }
      restartPolicy = local.restart_policy
      # In-flight requests may take up to MODEL_TIMEOUT_SECONDS; uvicorn drains them on
      # SIGTERM. 120 s is the Fargate maximum.
      stopTimeout      = 120
      logConfiguration = local.log_options.runtime
    })
  ])
}

resource "aws_ecs_service" "runtime" {
  name            = local.services.runtime
  cluster         = aws_ecs_cluster.main.id
  task_definition = aws_ecs_task_definition.runtime.arn
  desired_count   = var.runtime_desired_count
  launch_type     = var.launch_type

  # Fargate: start the new task before stopping the old one; Cloud Map then returns both
  # briefly. EC2: the fixed host port allows one task, so stop first (chat is unavailable
  # for the restart; the bot answers "can't reach my brain").
  deployment_minimum_healthy_percent = local.ec2 ? 0 : 100
  deployment_maximum_percent         = local.ec2 ? 100 : 200

  deployment_circuit_breaker {
    enable   = true
    rollback = true
  }

  dynamic "network_configuration" {
    for_each = local.fargate ? [1] : []
    content {
      subnets          = aws_subnet.public[*].id
      security_groups  = [aws_security_group.runtime[0].id]
      assign_public_ip = true # outbound only; the security group admits just the bot
    }
  }

  dynamic "service_registries" {
    for_each = local.fargate ? [1] : []
    content {
      registry_arn = aws_service_discovery_service.runtime[0].arn
    }
  }

  lifecycle {
    ignore_changes = [task_definition]
  }
}

# ---- Discord bot

resource "aws_ecs_task_definition" "discord" {
  family                   = local.services.discord
  requires_compatibilities = [var.launch_type]
  network_mode             = local.ec2 ? "bridge" : "awsvpc"
  cpu                      = local.ec2 ? null : var.discord_cpu
  memory                   = local.ec2 ? null : var.discord_memory
  execution_role_arn       = aws_iam_role.execution["discord"].arn
  task_role_arn            = one(aws_iam_role.discord_task[*].arn)

  # Writable scratch space for library uploads (download + transcode); the root
  # filesystem stays read-only.
  volume {
    name = "tmp"
  }

  dynamic "runtime_platform" {
    for_each = local.fargate ? [1] : []
    content {
      operating_system_family = "LINUX"
      cpu_architecture        = var.cpu_architecture
    }
  }

  dynamic "placement_constraints" {
    for_each = local.ec2 ? [1] : []
    content {
      type       = "memberOf"
      expression = "attribute:ecs.cpu-architecture == ${local.ecs_cpu_architecture}"
    }
  }

  container_definitions = jsonencode([
    merge(local.container_memory.discord, {
      name                   = "discord"
      image                  = "${aws_ecr_repository.service["discord"].repository_url}:${local.bootstrap_tag}"
      essential              = true
      readonlyRootFilesystem = true
      mountPoints            = [{ sourceVolume = "tmp", containerPath = "/tmp", readOnly = false }]
      environment = concat(
        [
          { name = "AGENT_BASE_URL", value = local.runtime_url },
          { name = "AGENT_TIMEOUT_SECS", value = tostring(var.agent_timeout_secs) },
          { name = "RUST_LOG", value = "info,serenity=warn" },
        ],
        var.music_library_enabled ? [
          { name = "MUSIC_S3_BUCKET", value = local.music_bucket },
          { name = "MUSIC_S3_PREFIX", value = var.music_library_prefix },
          { name = "AWS_REGION", value = var.region },
          { name = "MUSIC_GUILD_QUOTA_MB", value = tostring(var.music_guild_quota_mb) },
          { name = "MUSIC_UPLOAD_ROLE", value = var.music_upload_role },
        ] : [],
      )
      secrets = concat(
        [
          { name = "DISCORD_TOKEN", valueFrom = local.secret_arns.discord_token },
          { name = "AGENT_API_KEY", valueFrom = local.secret_arns.agent_api_key },
        ],
        # Supabase via Supavisor session mode, e.g.
        # postgres://postgres.<ref>:<pw>@aws-1-ap-southeast-1.pooler.supabase.com:5432/postgres
        #   ?sslmode=verify-full&sslrootcert=/etc/tatarbot/supabase-root-2021.crt
        var.music_library_enabled ? [
          { name = "DATABASE_URL", valueFrom = local.secret_arns.database_url },
        ] : [],
      )
      restartPolicy = local.restart_policy
      # The bot closes its gateway connection on SIGTERM within a few seconds.
      stopTimeout      = 30
      logConfiguration = local.log_options.discord
    })
  ])
}

resource "aws_ecs_service" "discord" {
  name            = local.services.discord
  cluster         = aws_ecs_cluster.main.id
  task_definition = aws_ecs_task_definition.discord.arn
  desired_count   = var.discord_desired_count
  launch_type     = var.launch_type

  # Stop the old bot before starting the new one: two connected bots reply twice.
  deployment_minimum_healthy_percent = 0
  deployment_maximum_percent         = 100

  deployment_circuit_breaker {
    enable   = true
    rollback = true
  }

  dynamic "network_configuration" {
    for_each = local.fargate ? [1] : []
    content {
      subnets          = aws_subnet.public[*].id
      security_groups  = [aws_security_group.discord[0].id]
      assign_public_ip = true
    }
  }

  lifecycle {
    ignore_changes = [task_definition]
  }
}

locals {
  ecs_services = {
    discord = aws_ecs_service.discord
    runtime = aws_ecs_service.runtime
  }
}
