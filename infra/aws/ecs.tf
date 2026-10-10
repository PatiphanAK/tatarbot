# Two independent ECS services on one cluster: tatarbot-discord and tatarbot-runtime.
#
# Terraform owns the services and the *initial* task definitions. The deploy workflows
# register new task-definition revisions with new image tags, so the services ignore
# task_definition drift.

locals {
  runtime_url = "http://${aws_service_discovery_service.runtime.name}.${aws_service_discovery_private_dns_namespace.main.name}:${var.runtime_port}"

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
  requires_compatibilities = ["FARGATE"]
  network_mode             = "awsvpc"
  cpu                      = var.runtime_cpu
  memory                   = var.runtime_memory
  execution_role_arn       = aws_iam_role.execution["runtime"].arn

  runtime_platform {
    operating_system_family = "LINUX"
    cpu_architecture        = var.cpu_architecture
  }

  container_definitions = jsonencode([
    {
      name                   = "runtime"
      image                  = "${aws_ecr_repository.service["runtime"].repository_url}:${local.bootstrap_tag}"
      essential              = true
      readonlyRootFilesystem = true
      portMappings           = [{ containerPort = var.runtime_port, protocol = "tcp", name = "http" }]
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
      # In-flight requests may take up to MODEL_TIMEOUT_SECONDS; uvicorn drains them on
      # SIGTERM. 120 s is the Fargate maximum.
      stopTimeout      = 120
      logConfiguration = local.log_options.runtime
    }
  ])
}

resource "aws_ecs_service" "runtime" {
  name            = local.services.runtime
  cluster         = aws_ecs_cluster.main.id
  task_definition = aws_ecs_task_definition.runtime.arn
  desired_count   = var.runtime_desired_count
  launch_type     = "FARGATE"

  # Start the new task before stopping the old one; Cloud Map then returns both briefly.
  deployment_minimum_healthy_percent = 100
  deployment_maximum_percent         = 200

  deployment_circuit_breaker {
    enable   = true
    rollback = true
  }

  network_configuration {
    subnets          = aws_subnet.public[*].id
    security_groups  = [aws_security_group.runtime.id]
    assign_public_ip = true # outbound only; the security group admits just the bot
  }

  service_registries {
    registry_arn = aws_service_discovery_service.runtime.arn
  }

  lifecycle {
    ignore_changes = [task_definition]
  }
}

# ---- Discord bot

resource "aws_ecs_task_definition" "discord" {
  family                   = local.services.discord
  requires_compatibilities = ["FARGATE"]
  network_mode             = "awsvpc"
  cpu                      = var.discord_cpu
  memory                   = var.discord_memory
  execution_role_arn       = aws_iam_role.execution["discord"].arn
  task_role_arn            = one(aws_iam_role.discord_task[*].arn)

  # Writable scratch space for library uploads (download + transcode); the root
  # filesystem stays read-only.
  volume {
    name = "tmp"
  }

  runtime_platform {
    operating_system_family = "LINUX"
    cpu_architecture        = var.cpu_architecture
  }

  container_definitions = jsonencode([
    {
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
      # The bot closes its gateway connection on SIGTERM within a few seconds.
      stopTimeout      = 30
      logConfiguration = local.log_options.discord
    }
  ])
}

resource "aws_ecs_service" "discord" {
  name            = local.services.discord
  cluster         = aws_ecs_cluster.main.id
  task_definition = aws_ecs_task_definition.discord.arn
  desired_count   = var.discord_desired_count
  launch_type     = "FARGATE"

  # Stop the old bot before starting the new one: two connected bots reply twice.
  deployment_minimum_healthy_percent = 0
  deployment_maximum_percent         = 100

  deployment_circuit_breaker {
    enable   = true
    rollback = true
  }

  network_configuration {
    subnets          = aws_subnet.public[*].id
    security_groups  = [aws_security_group.discord.id]
    assign_public_ip = true
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
