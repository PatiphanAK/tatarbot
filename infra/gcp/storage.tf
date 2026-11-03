# Music library (only with music_library_enabled): a private bucket and a dedicated service
# account whose HMAC key the bot uses through GCS's S3-compatible XML API, so the bot's
# aws-sdk-s3 code runs unchanged (endpoint https://storage.googleapis.com, path style).
# The HMAC key is created by hand (README) so its secret never lands in Terraform state.
# Metadata stays in Supabase Postgres; object keys are the same as on S3.

locals {
  music_bucket = coalesce(var.music_library_bucket, "${var.name}-music-${var.project_id}")
}

resource "google_storage_bucket" "music" {
  count = var.music_library_enabled ? 1 : 0

  name                        = local.music_bucket
  location                    = upper(var.region)
  storage_class               = "STANDARD"
  uniform_bucket_level_access = true
  public_access_prevention    = "enforced"
  force_destroy               = false

  soft_delete_policy {
    retention_duration_seconds = var.music_soft_delete_days * 86400
  }

  lifecycle_rule {
    condition {
      age = 1
    }
    action {
      type = "AbortIncompleteMultipartUpload"
    }
  }

  depends_on = [google_project_service.api]
}

resource "google_service_account" "music" {
  count = var.music_library_enabled ? 1 : 0

  account_id   = "${var.name}-music"
  display_name = "TatarBot music library (HMAC key for the bot)"
}

# Read, write, delete and list objects in this bucket only. Presigned GET URLs are signed
# with the HMAC key, so playback needs nothing more.
resource "google_storage_bucket_iam_member" "music" {
  count = var.music_library_enabled ? 1 : 0

  bucket = google_storage_bucket.music[0].name
  role   = "roles/storage.objectUser"
  member = google_service_account.music[0].member
}
