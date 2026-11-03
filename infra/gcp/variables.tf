variable "project_id" {
  description = "GCP project that holds every resource. Billing must be enabled on it."
  type        = string
}

variable "region" {
  description = "Region for every regional resource. asia-southeast1 (Singapore) is close to Supabase, the Qwen endpoint and the Discord users."
  type        = string
  default     = "asia-southeast1"
}

variable "zone" {
  description = "Zone of the VM."
  type        = string
  default     = "asia-southeast1-b"
}

variable "name" {
  description = "Name prefix for resources."
  type        = string
  default     = "tatarbot"
}

variable "subnet_cidr" {
  description = "Primary range of the only subnet."
  type        = string
  default     = "10.50.0.0/24"
}

# ---- VM

variable "machine_type" {
  description = "e2-micro (1 GB, shared 2 vCPU) fits both services like the t4g.micro did. e2-small (2 GB) if memory runs short."
  type        = string
  default     = "e2-micro"
}

variable "boot_disk_gb" {
  description = "Boot disk size. Container-Optimized OS needs 10 GB."
  type        = number
  default     = 10

  validation {
    condition     = var.boot_disk_gb >= 10
    error_message = "boot_disk_gb must be at least 10 (the COS image size)."
  }
}

variable "boot_disk_type" {
  description = "pd-balanced (default) or pd-standard (about half the price, slower image pulls)."
  type        = string
  default     = "pd-balanced"
}

variable "cos_image" {
  description = "Boot image. The family is resolved when the VM is created; COS then updates itself in place."
  type        = string
  default     = "projects/cos-cloud/global/images/family/cos-stable"
}

variable "docker_subnet" {
  description = "Subnet of the Docker network both containers join. Containers in it can't reach the metadata server."
  type        = string
  default     = "172.30.0.0/24"
}

variable "network_tier" {
  description = "Tier of the VM's external IP. STANDARD: the first 200 GiB/month of internet egress from Singapore are free (account-wide), and traffic leaves Google's network in Singapore, where Discord, Supabase and Qwen are. PREMIUM: $0.12/GiB from the first byte."
  type        = string
  default     = "STANDARD"

  validation {
    condition     = contains(["STANDARD", "PREMIUM"], var.network_tier)
    error_message = "network_tier must be STANDARD or PREMIUM."
  }
}

# ---- Discord bot (the VM). Starts only after its first deploy writes an image reference.

variable "discord_enabled" {
  description = "Run the Discord bot container. Never run it while another bot uses the same token (AWS)."
  type        = bool
  default     = true
}

variable "discord_memory_mb" {
  description = "Hard memory limit of the bot container, including ffmpeg during uploads. The VM runs nothing else, so up to ~650 fits an e2-micro."
  type        = number
  default     = 512
}

variable "bot_container_uid" {
  description = "UID the bot image runs as (distroless nonroot). Only it (and root) can read the ID-token file."
  type        = number
  default     = 65532
}

variable "id_token_refresh_minutes" {
  description = "How often the host fetches a new ID token for the runtime. Google ID tokens last 60 minutes."
  type        = number
  default     = 10

  validation {
    condition     = var.id_token_refresh_minutes >= 1 && var.id_token_refresh_minutes <= 30
    error_message = "id_token_refresh_minutes must be 1-30, leaving at least 30 minutes of validity."
  }
}

# ---- Agent Runtime (Cloud Run)

variable "runtime_cpu" {
  description = "vCPU per Cloud Run instance. 1 keeps any concurrency allowed; CPU is billed only while requests run (request-based billing)."
  type        = string
  default     = "1"
}

variable "runtime_memory" {
  description = "Memory per Cloud Run instance. Measured: ~82 MiB idle."
  type        = string
  default     = "512Mi"
}

variable "runtime_min_instances" {
  description = "0 scales to zero when idle (cold start on the next message). 1 keeps one warm, billed at the idle min-instance rate."
  type        = number
  default     = 0
}

variable "runtime_max_instances" {
  description = "Upper bound on instances. 1 keeps the per-conversation turn lock in one process; history is in Postgres either way."
  type        = number
  default     = 1
}

variable "runtime_concurrency" {
  description = "Concurrent requests per instance. The runtime is async; turns mostly wait on the model."
  type        = number
  default     = 10
}

variable "runtime_request_timeout_seconds" {
  description = "Cloud Run request timeout. Must exceed agent_timeout_secs so the bot, not Cloud Run, gives up first."
  type        = number
  default     = 150
}

variable "runtime_history_store" {
  description = "postgres: history survives scale-to-zero (needs the runtime-database-url secret and the migrations). memory: lost whenever the instance stops."
  type        = string
  default     = "postgres"

  validation {
    condition     = contains(["postgres", "memory"], var.runtime_history_store)
    error_message = "runtime_history_store must be postgres or memory."
  }
}

variable "conversation_retention_days" {
  description = "Conversations idle longer than this are deleted from Postgres."
  type        = number
  default     = 30
}

variable "runtime_port" {
  type    = number
  default = 8080
}

variable "model_base_url" {
  description = "OpenAI-compatible API root."
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
  description = "Bot -> runtime timeout, including a cold start."
  type        = number
  default     = 120
}

variable "web_search_enabled" {
  description = "Give the runtime TAVILY_API_KEY (the secret must have a version), enabling web search."
  type        = bool
  default     = true
}

variable "runtime_extra_environment" {
  description = "Further non-secret runtime settings, e.g. { MODEL_ENABLE_THINKING = \"false\" }."
  type        = map(string)
  default     = {}
}

# ---- Music library

variable "music_library_enabled" {
  description = "Server music library: GCS bucket, its service account and the DATABASE_URL + HMAC secrets (they must have versions)."
  type        = bool
  default     = false
}

variable "music_library_bucket" {
  description = "Bucket for library audio; empty means tatarbot-music-<project id>."
  type        = string
  default     = ""
}

variable "music_library_prefix" {
  description = "Key prefix for library objects. Must match the keys stored in Postgres."
  type        = string
  default     = "library/"

  validation {
    condition     = can(regex("^[a-z0-9][a-z0-9/_-]*/$", var.music_library_prefix))
    error_message = "music_library_prefix must end with / and not start with /."
  }
}

variable "music_guild_quota_mb" {
  description = "Library storage per Discord server, in MB."
  type        = number
  default     = 500
}

variable "music_upload_role" {
  description = "Discord role name (per server) that may upload and delete others' tracks."
  type        = string
  default     = "DJ"
}

variable "music_soft_delete_days" {
  description = "GCS soft-delete retention. 0 keeps the AWS behaviour: a delete is final (the library refcounts objects)."
  type        = number
  default     = 0
}

# ---- Registry, logs

variable "registry_keep_images" {
  description = "Tagged images kept per repository (the rollback window). 3 keeps both repositories well under 0.5 GB."
  type        = number
  default     = 3
}


# ---- Deployment from GitHub Actions

variable "github_repository" {
  description = "owner/name of the repository allowed to deploy."
  type        = string
  default     = "PatiphanAK/tatarbot"
}

variable "github_environment" {
  description = "GitHub environment the deploy jobs run in; the only one Workload Identity Federation trusts."
  type        = string
  default     = "production"
}

# ---- Cost guard

variable "billing_account" {
  description = "Billing account id (XXXXXX-XXXXXX-XXXXXX) for the budget. Empty skips the budget."
  type        = string
  default     = ""
}

variable "budget_amount" {
  description = "Monthly budget in the billing account's currency (e.g. 20 for USD, 700 for THB); alerts go to the billing account's admins at 50%, 90% and 100% (actual) and 100% (forecast)."
  type        = number
  default     = 20
}
