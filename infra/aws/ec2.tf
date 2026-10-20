# ECS container capacity for launch_type = "EC2": one Graviton instance in an Auto Scaling
# Group of exactly 0 or 1.
#
# * The ASG replaces the instance if it fails an EC2 status check (a few minutes without
#   the bot and the runtime). There is no second instance: this is not highly available.
# * Nothing on the instance needs to survive: images come from ECR, secrets from SSM,
#   library data from Postgres + S3. The root volume is deleted with the instance, so
#   scaling to 0 (ec2_desired_capacity or ec2_schedule) stops all of its charges.
# * The AMI is the latest ECS-optimized Amazon Linux 2023 image, read from AWS's public SSM
#   parameter at plan time. A new AMI shows up as a launch-template change; applying it
#   replaces the instance (instance refresh). That is how the host gets patched.
# * No SSH: no key pair, no inbound rule. Shell access is Session Manager.

data "aws_ssm_parameter" "ecs_ami" {
  count = local.ec2 ? 1 : 0

  name = (var.cpu_architecture == "ARM64"
    ? "/aws/service/ecs/optimized-ami/amazon-linux-2023/arm64/recommended/image_id"
    : "/aws/service/ecs/optimized-ami/amazon-linux-2023/recommended/image_id"
  )
}

# ---- Instance role: the ECS agent (register, poll, pull images, ship logs) and the SSM
# agent (Session Manager). Both are AWS-managed policies, kept current by AWS as the agents
# change. Containers can't use this role: IMDSv2 with a hop limit of 1 is unreachable from
# bridge-networked containers, which get their own task roles from the ECS agent instead.

data "aws_iam_policy_document" "ec2_assume" {
  statement {
    actions = ["sts:AssumeRole"]
    principals {
      type        = "Service"
      identifiers = ["ec2.amazonaws.com"]
    }
  }
}

resource "aws_iam_role" "ecs_instance" {
  count = local.ec2 ? 1 : 0

  name               = "${var.project}-ecs-instance"
  assume_role_policy = data.aws_iam_policy_document.ec2_assume.json
}

resource "aws_iam_role_policy_attachment" "ecs_instance" {
  for_each = local.ec2 ? toset([
    "arn:aws:iam::aws:policy/service-role/AmazonEC2ContainerServiceforEC2Role",
    "arn:aws:iam::aws:policy/AmazonSSMManagedInstanceCore",
  ]) : toset([])

  role       = aws_iam_role.ecs_instance[0].name
  policy_arn = each.value
}

resource "aws_iam_instance_profile" "ecs_instance" {
  count = local.ec2 ? 1 : 0

  name = "${var.project}-ecs-instance"
  role = aws_iam_role.ecs_instance[0].name
}

# ---- Launch template and Auto Scaling Group

resource "aws_launch_template" "ecs" {
  count = local.ec2 ? 1 : 0

  name_prefix            = "${var.project}-ecs-"
  image_id               = data.aws_ssm_parameter.ecs_ami[0].insecure_value
  instance_type          = var.ec2_instance_type
  update_default_version = true

  iam_instance_profile {
    arn = aws_iam_instance_profile.ecs_instance[0].arn
  }

  # Public IPv4 for outbound traffic only (no NAT gateway); the security group has no
  # inbound rules.
  network_interfaces {
    device_index                = 0
    associate_public_ip_address = true
    security_groups             = [aws_security_group.ecs_host[0].id]
    delete_on_termination       = true
  }

  metadata_options {
    http_endpoint               = "enabled"
    http_tokens                 = "required"
    http_put_response_hop_limit = 1 # host only: containers can't reach IMDS
    instance_metadata_tags      = "disabled"
  }

  block_device_mappings {
    device_name = "/dev/xvda"
    ebs {
      volume_size           = var.ec2_root_volume_gb
      volume_type           = "gp3"
      encrypted             = true
      delete_on_termination = true
    }
  }

  credit_specification {
    cpu_credits = var.ec2_cpu_credits
  }

  monitoring {
    enabled = false # detailed monitoring bills per metric; basic 5-minute metrics are free
  }

  # Join the cluster; the SSM agent is installed if this AMI build lacks it.
  user_data = base64encode(<<-EOT
    #!/bin/bash
    set -eu
    cat >> /etc/ecs/ecs.config <<'EOF'
    ECS_CLUSTER=${aws_ecs_cluster.main.name}
    ECS_ENABLE_TASK_IAM_ROLE=true
    ECS_ENGINE_TASK_CLEANUP_WAIT_DURATION=1h
    ECS_IMAGE_PULL_BEHAVIOR=prefer-cached
    EOF
    rpm -q amazon-ssm-agent >/dev/null || dnf install -y amazon-ssm-agent
    systemctl enable --now amazon-ssm-agent
  EOT
  )

  tag_specifications {
    resource_type = "instance"
    tags          = { Name = "${var.project}-ecs" }
  }

  tag_specifications {
    resource_type = "volume"
    tags          = { Name = "${var.project}-ecs" }
  }

  lifecycle {
    precondition {
      # Graviton types have a "g" after the generation number: t4g, m7g, c7gn, r8gd.
      condition     = (var.cpu_architecture == "ARM64") == can(regex("^[a-z]+[0-9]+g", var.ec2_instance_type))
      error_message = "ec2_instance_type must match cpu_architecture (a Graviton type such as t4g.micro for ARM64)."
    }
  }
}

resource "aws_autoscaling_group" "ecs" {
  count = local.ec2 ? 1 : 0

  name     = "${var.project}-ecs"
  min_size = 0
  max_size = 1
  # With a schedule, the scheduled actions own the instance count and Terraform leaves it
  # alone (null keeps the current value; a new group starts at min_size 0).
  desired_capacity          = var.ec2_schedule == null ? var.ec2_desired_capacity : null
  vpc_zone_identifier       = aws_subnet.public[*].id
  health_check_type         = "EC2"
  health_check_grace_period = 120

  launch_template {
    id      = aws_launch_template.ecs[0].id
    version = aws_launch_template.ecs[0].latest_version
  }

  # A new launch template version (new AMI, instance type, user data) replaces the
  # instance: terminate, then launch. With one instance that is a few minutes offline.
  instance_refresh {
    strategy = "Rolling"
    preferences {
      min_healthy_percentage = 0
    }
  }

  tag {
    key                 = "Name"
    value               = "${var.project}-ecs"
    propagate_at_launch = true
  }
}

# Optional daily on-hours. Off means the instance and its volume are deleted; the ECS
# services keep desired count 1 and their tasks start again when the instance registers.
# Terraform then ignores ec2_desired_capacity, so an apply never starts or stops the
# instance; outside the schedule, use `aws autoscaling set-desired-capacity`.

resource "aws_autoscaling_schedule" "start" {
  count = local.ec2 && var.ec2_schedule != null ? 1 : 0

  scheduled_action_name  = "${var.project}-start"
  autoscaling_group_name = aws_autoscaling_group.ecs[0].name
  recurrence             = var.ec2_schedule.start
  time_zone              = var.ec2_schedule.time_zone
  min_size               = 0
  max_size               = 1
  desired_capacity       = 1
}

resource "aws_autoscaling_schedule" "stop" {
  count = local.ec2 && var.ec2_schedule != null ? 1 : 0

  scheduled_action_name  = "${var.project}-stop"
  autoscaling_group_name = aws_autoscaling_group.ecs[0].name
  recurrence             = var.ec2_schedule.stop
  time_zone              = var.ec2_schedule.time_zone
  min_size               = 0
  max_size               = 1
  desired_capacity       = 0
}
