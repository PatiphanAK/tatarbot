# Music library (Phase 1.5), only when music_library_enabled:
#
# * A private S3 bucket for the audio (Ogg Opus objects under music_library_prefix).
# * The bot's first task role: read/write/delete objects under that prefix and list it
#   (orphan sweep). Presigned GET URLs are signed with this role, so playback needs no
#   extra permission. Nothing else in the account is reachable from the bot.
# * Metadata lives in Postgres (Supabase); DATABASE_URL comes from SSM like the other
#   secrets (iam.tf, ecs.tf).

locals {
  music_bucket = coalesce(var.music_library_bucket, "tatarbot-music-${local.account_id}")
}

resource "aws_s3_bucket" "music" {
  count  = var.music_library_enabled ? 1 : 0
  bucket = local.music_bucket
}

resource "aws_s3_bucket_public_access_block" "music" {
  count                   = var.music_library_enabled ? 1 : 0
  bucket                  = aws_s3_bucket.music[0].id
  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

resource "aws_s3_bucket_ownership_controls" "music" {
  count  = var.music_library_enabled ? 1 : 0
  bucket = aws_s3_bucket.music[0].id
  rule {
    object_ownership = "BucketOwnerEnforced"
  }
}

resource "aws_s3_bucket_server_side_encryption_configuration" "music" {
  count  = var.music_library_enabled ? 1 : 0
  bucket = aws_s3_bucket.music[0].id
  rule {
    apply_server_side_encryption_by_default {
      sse_algorithm = "AES256"
    }
  }
}

resource "aws_s3_bucket_lifecycle_configuration" "music" {
  count  = var.music_library_enabled ? 1 : 0
  bucket = aws_s3_bucket.music[0].id
  rule {
    id     = "abort-incomplete-uploads"
    status = "Enabled"
    filter {}
    abort_incomplete_multipart_upload {
      days_after_initiation = 1
    }
  }
}

data "aws_iam_policy_document" "music_bucket" {
  count = var.music_library_enabled ? 1 : 0
  statement {
    sid       = "DenyInsecureTransport"
    effect    = "Deny"
    actions   = ["s3:*"]
    resources = [aws_s3_bucket.music[0].arn, "${aws_s3_bucket.music[0].arn}/*"]
    principals {
      type        = "*"
      identifiers = ["*"]
    }
    condition {
      test     = "Bool"
      variable = "aws:SecureTransport"
      values   = ["false"]
    }
  }
}

resource "aws_s3_bucket_policy" "music" {
  count      = var.music_library_enabled ? 1 : 0
  bucket     = aws_s3_bucket.music[0].id
  policy     = data.aws_iam_policy_document.music_bucket[0].json
  depends_on = [aws_s3_bucket_public_access_block.music]
}

resource "aws_iam_role" "discord_task" {
  count              = var.music_library_enabled ? 1 : 0
  name               = "${local.services.discord}-task"
  assume_role_policy = data.aws_iam_policy_document.ecs_tasks_assume.json
}

data "aws_iam_policy_document" "discord_task" {
  count = var.music_library_enabled ? 1 : 0
  statement {
    sid       = "LibraryObjects"
    actions   = ["s3:GetObject", "s3:PutObject", "s3:DeleteObject"]
    resources = ["${aws_s3_bucket.music[0].arn}/${var.music_library_prefix}*"]
  }
  statement {
    sid       = "ListLibraryPrefix"
    actions   = ["s3:ListBucket"]
    resources = [aws_s3_bucket.music[0].arn]
    condition {
      test     = "StringLike"
      variable = "s3:prefix"
      values   = ["${var.music_library_prefix}*"]
    }
  }
}

resource "aws_iam_role_policy" "discord_task" {
  count  = var.music_library_enabled ? 1 : 0
  name   = "music-library"
  role   = aws_iam_role.discord_task[0].id
  policy = data.aws_iam_policy_document.discord_task[0].json
}
