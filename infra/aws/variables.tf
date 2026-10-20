variable "region" {
  description = "AWS region for every resource."
  type        = string
  default     = "ap-southeast-1"
}

variable "project" {
  description = "Name prefix for resources."
  type        = string
  default     = "tatarbot"
}

variable "vpc_cidr" {
  description = "CIDR of the dedicated VPC. Two public /24 subnets are carved from it."
  type        = string
  default     = "10.40.0.0/16"
}

variable "launch_type" {
  description = "EC2: both services on one Graviton instance (cheapest always-on). FARGATE: one task each."
  type        = string
  default     = "EC2"

  validation {
    condition     = contains(["EC2", "FARGATE"], var.launch_type)
    error_message = "launch_type must be EC2 or FARGATE."
  }
}

variable "cpu_architecture" {
  description = "CPU architecture of the tasks (and of the EC2 instance). Must match the images (CI builds linux/arm64)."
  type        = string
  default     = "ARM64"

  validation {
    condition     = contains(["X86_64", "ARM64"], var.cpu_architecture)
    error_message = "cpu_architecture must be X86_64 or ARM64."
  }
}

# ---- Service sizing and scale. Start at 0 until images exist (see README).

variable "runtime_desired_count" {
  description = "Agent Runtime tasks. Keep at most 1: history is in memory per task."
  type        = number
  default     = 0

  validation {
    condition     = var.runtime_desired_count >= 0 && var.runtime_desired_count <= 1
    error_message = "runtime_desired_count must be 0 or 1 while history is in memory."
  }
}

variable "discord_desired_count" {
  description = "Discord bot tasks. Never more than 1: two bots on one token reply twice."
  type        = number
  default     = 0

  validation {
    condition     = var.discord_desired_count >= 0 && var.discord_desired_count <= 1
    error_message = "discord_desired_count must be 0 or 1."
  }
}

variable "runtime_cpu" {
  description = "Fargate task CPU units. Unused on EC2 (containers share the instance)."
  type        = number
  default     = 256
}

variable "runtime_memory" {
  description = "Fargate task memory (MiB); on EC2, the container's hard limit."
  type        = number
  default     = 512
}

variable "runtime_memory_reservation" {
  description = "EC2 only: memory (MiB) ECS reserves for placement. Measured idle: ~70 MiB."
  type        = number
  default     = 192
}

variable "discord_cpu" {
  description = "Fargate task CPU units. Unused on EC2 (containers share the instance)."
  type        = number
  default     = 256
}

variable "discord_memory" {
  description = "Fargate task memory (MiB); on EC2, the container's hard limit. Includes ffmpeg during uploads."
  type        = number
  default     = 512
}

variable "discord_memory_reservation" {
  description = "EC2 only: memory (MiB) ECS reserves for placement."
  type        = number
  default     = 192
}

# ---- EC2 capacity (launch_type = "EC2")

variable "ec2_instance_type" {
  description = "Container instance type. t4g.micro (1 GiB) fits both services; t4g.nano (0.5 GiB) does not."
  type        = string
  default     = "t4g.micro"
}

variable "ec2_cpu_credits" {
  description = "T-instance credit mode. unlimited never throttles but bills sustained use above baseline; standard caps cost and throttles."
  type        = string
  default     = "unlimited"

  validation {
    condition     = contains(["standard", "unlimited"], var.ec2_cpu_credits)
    error_message = "ec2_cpu_credits must be standard or unlimited."
  }
}

variable "ec2_root_volume_gb" {
  description = "Root gp3 volume. The ECS-optimized AL2023 AMI snapshot is 30 GB, the minimum."
  type        = number
  default     = 30

  validation {
    condition     = var.ec2_root_volume_gb >= 30
    error_message = "ec2_root_volume_gb must be at least 30 (the AMI snapshot size)."
  }
}

variable "ec2_desired_capacity" {
  description = "Instances to run: 1, or 0 to stop everything (the instance and its volume are deleted). Ignored when ec2_schedule is set."
  type        = number
  default     = 1

  validation {
    condition     = contains([0, 1], var.ec2_desired_capacity)
    error_message = "ec2_desired_capacity must be 0 or 1."
  }
}

variable "ec2_schedule" {
  description = "Optional daily on-hours, as cron in time_zone, e.g. { start = \"0 18 * * *\", stop = \"0 23 * * *\" }. null runs 24/7."
  type = object({
    start     = string
    stop      = string
    time_zone = optional(string, "Asia/Bangkok")
  })
  default = null
}

variable "ec2_docker_bridge_ip" {
  description = "EC2 only: Docker bridge gateway on the instance, where the runtime's host port is reached."
  type        = string
  default     = "172.17.0.1"
}

# ---- Application configuration (non-secret). Secrets live in SSM, see ssm_prefix.

variable "runtime_port" {
  type    = number
  default = 8080
}

variable "model_base_url" {
  description = "OpenAI-compatible API root, e.g. https://api.runpod.ai/v2/<ENDPOINT_ID>/openai/v1."
  type        = string
}

variable "model_name" {
  description = "Model id served by the backend (must appear in GET /models)."
  type        = string
}

variable "model_timeout_seconds" {
  description = "Runtime -> model timeout. Must stay below agent_timeout_secs."
  type        = number
  default     = 100
}

variable "agent_timeout_secs" {
  description = "Bot -> runtime timeout."
  type        = number
  default     = 120
}

variable "web_search_enabled" {
  description = "Give the runtime TAVILY_API_KEY from SSM (the parameter must exist), enabling web search."
  type        = bool
  default     = true
}

variable "music_library_enabled" {
  description = "Server music library: S3 bucket, bot task role, DATABASE_URL from SSM (the parameter must exist)."
  type        = bool
  default     = false
}

variable "music_library_bucket" {
  description = "Bucket name for library audio; empty means tatarbot-music-<account id>."
  type        = string
  default     = ""
}

variable "music_library_prefix" {
  description = "Key prefix for library objects; the task role is scoped to it."
  type        = string
  default     = "library/"
  validation {
    condition     = can(regex("^[a-z0-9][a-z0-9/_-]*/$", var.music_library_prefix))
    error_message = "music_library_prefix must end with / and not start with /."
  }
}

variable "music_guild_quota_mb" {
  description = "Library storage per Discord server, in MB (each server counts shared files in full)."
  type        = number
  default     = 500
}

variable "music_upload_role" {
  description = "Discord role name (per server) that may upload and delete others' tracks."
  type        = string
  default     = "DJ"
}

variable "runtime_extra_environment" {
  description = "Further non-secret runtime settings, e.g. { MODEL_ENABLE_THINKING = \"false\" }."
  type        = map(string)
  default     = {}
}

variable "ssm_prefix" {
  description = "SSM Parameter Store path holding the SecureString secrets (created outside Terraform)."
  type        = string
  default     = "/tatarbot"
}

variable "log_retention_days" {
  type    = number
  default = 14
}

variable "ecr_keep_images" {
  description = "Images kept per repository (the rollback window)."
  type        = number
  default     = 10
}

# ---- Deployment from GitHub Actions

variable "github_repository" {
  description = "owner/name of the repository allowed to deploy."
  type        = string
  default     = "PatiphanAK/tatarbot"
}

variable "github_environment" {
  description = "GitHub environment the deploy jobs run in; the only OIDC subject trusted."
  type        = string
  default     = "production"
}

variable "create_github_oidc_provider" {
  description = "false if the account already has the token.actions.githubusercontent.com provider."
  type        = bool
  default     = true
}

# ---- Cost guard

variable "budget_limit_usd" {
  description = "Monthly AWS budget; alerts at 80% actual and 100% forecast. EC2 24/7 is ~$15 (README)."
  type        = number
  default     = 20
}

variable "budget_email" {
  description = "Alert recipient. Empty disables the budget."
  type        = string
  default     = ""
}
