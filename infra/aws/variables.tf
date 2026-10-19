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

variable "cpu_architecture" {
  description = "Fargate CPU architecture. Must match the images (CI builds linux/arm64)."
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
  type    = number
  default = 256
}

variable "runtime_memory" {
  type    = number
  default = 512
}

variable "discord_cpu" {
  type    = number
  default = 256
}

variable "discord_memory" {
  type    = number
  default = 512
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
  description = "Monthly AWS budget; alerts at 80% actual and 100% forecast."
  type        = number
  default     = 15
}

variable "budget_email" {
  description = "Alert recipient. Empty disables the budget."
  type        = string
  default     = ""
}
