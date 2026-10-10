# Least-privilege roles.
#
# * One execution role per service: pull its own image, write its own logs, read only its
#   own secrets. The runtime has no task role (it calls no AWS API); the bot gets one only
#   with the music library, scoped to its bucket prefix (music.tf).
# * One deploy role for GitHub Actions (OIDC, no stored keys), trusted only for the
#   configured repository *and* GitHub environment.

locals {
  account_id = data.aws_caller_identity.current.account_id

  # SecureString parameters created by the operator (see README); Terraform never sees values.
  secret_names = {
    discord_token  = "${var.ssm_prefix}/discord/DISCORD_TOKEN"
    agent_api_key  = "${var.ssm_prefix}/shared/AGENT_API_KEY"
    model_api_key  = "${var.ssm_prefix}/runtime/MODEL_API_KEY"
    tavily_api_key = "${var.ssm_prefix}/runtime/TAVILY_API_KEY"
    database_url   = "${var.ssm_prefix}/discord/DATABASE_URL"
  }
  secret_arns = {
    for k, name in local.secret_names :
    k => "arn:aws:ssm:${var.region}:${local.account_id}:parameter${name}"
  }

  service_secrets = {
    discord = concat(
      [local.secret_arns.discord_token, local.secret_arns.agent_api_key],
      var.music_library_enabled ? [local.secret_arns.database_url] : [],
    )
    runtime = concat(
      [local.secret_arns.agent_api_key, local.secret_arns.model_api_key],
      var.web_search_enabled ? [local.secret_arns.tavily_api_key] : [],
    )
  }
}

data "aws_iam_policy_document" "ecs_tasks_assume" {
  statement {
    actions = ["sts:AssumeRole"]
    principals {
      type        = "Service"
      identifiers = ["ecs-tasks.amazonaws.com"]
    }
    condition {
      test     = "StringEquals"
      variable = "aws:SourceAccount"
      values   = [local.account_id]
    }
  }
}

resource "aws_iam_role" "execution" {
  for_each = local.services

  name               = "${each.value}-execution"
  assume_role_policy = data.aws_iam_policy_document.ecs_tasks_assume.json
}

data "aws_iam_policy_document" "execution" {
  for_each = local.services

  statement {
    sid       = "EcrAuth"
    actions   = ["ecr:GetAuthorizationToken"]
    resources = ["*"] # does not support resource-level permissions
  }
  statement {
    sid       = "PullOwnImage"
    actions   = ["ecr:BatchCheckLayerAvailability", "ecr:BatchGetImage", "ecr:GetDownloadUrlForLayer"]
    resources = [aws_ecr_repository.service[each.key].arn]
  }
  statement {
    sid       = "WriteOwnLogs"
    actions   = ["logs:CreateLogStream", "logs:PutLogEvents"]
    resources = ["${aws_cloudwatch_log_group.service[each.key].arn}:*"]
  }
  statement {
    sid       = "ReadOwnSecrets"
    actions   = ["ssm:GetParameters"]
    resources = local.service_secrets[each.key]
  }
  # Parameters encrypted with the AWS-managed aws/ssm key need no kms:Decrypt grant here.
  # Add one scoped to the key ARN if a customer-managed key is used.
}

resource "aws_iam_role_policy" "execution" {
  for_each = local.services

  name   = "execution"
  role   = aws_iam_role.execution[each.key].id
  policy = data.aws_iam_policy_document.execution[each.key].json
}

# ---- GitHub Actions deploy role

resource "aws_iam_openid_connect_provider" "github" {
  count = var.create_github_oidc_provider ? 1 : 0

  url            = "https://token.actions.githubusercontent.com"
  client_id_list = ["sts.amazonaws.com"]
}

data "aws_iam_openid_connect_provider" "github" {
  count = var.create_github_oidc_provider ? 0 : 1
  url   = "https://token.actions.githubusercontent.com"
}

locals {
  github_oidc_provider_arn = (var.create_github_oidc_provider
    ? aws_iam_openid_connect_provider.github[0].arn
  : data.aws_iam_openid_connect_provider.github[0].arn)
}

data "aws_iam_policy_document" "deploy_assume" {
  statement {
    actions = ["sts:AssumeRoleWithWebIdentity"]
    principals {
      type        = "Federated"
      identifiers = [local.github_oidc_provider_arn]
    }
    condition {
      test     = "StringEquals"
      variable = "token.actions.githubusercontent.com:aud"
      values   = ["sts.amazonaws.com"]
    }
    # Only jobs running in the protected environment of this repository. Pull requests
    # (including from forks) never run in it, so they cannot assume this role.
    condition {
      test     = "StringEquals"
      variable = "token.actions.githubusercontent.com:sub"
      values   = ["repo:${var.github_repository}:environment:${var.github_environment}"]
    }
  }
}

resource "aws_iam_role" "deploy" {
  name                 = "${var.project}-github-deploy"
  assume_role_policy   = data.aws_iam_policy_document.deploy_assume.json
  max_session_duration = 3600
}

data "aws_iam_policy_document" "deploy" {
  statement {
    sid       = "EcrAuth"
    actions   = ["ecr:GetAuthorizationToken"]
    resources = ["*"]
  }
  statement {
    sid = "PushImages"
    actions = [
      "ecr:BatchCheckLayerAvailability",
      "ecr:BatchGetImage",
      "ecr:CompleteLayerUpload",
      "ecr:DescribeImages",
      "ecr:GetDownloadUrlForLayer",
      "ecr:InitiateLayerUpload",
      "ecr:PutImage",
      "ecr:UploadLayerPart",
    ]
    resources = [for r in aws_ecr_repository.service : r.arn]
  }
  statement {
    sid = "TaskDefinitions"
    # Describe/Register do not support resource-level permissions.
    actions   = ["ecs:DescribeTaskDefinition", "ecs:RegisterTaskDefinition"]
    resources = ["*"]
  }
  statement {
    sid       = "UpdateOwnServices"
    actions   = ["ecs:DescribeServices", "ecs:UpdateService"]
    resources = [for s in local.ecs_services : s.id]
  }
  statement {
    sid       = "PassServiceRoles"
    actions   = ["iam:PassRole"]
    resources = concat([for r in aws_iam_role.execution : r.arn], aws_iam_role.discord_task[*].arn)
    condition {
      test     = "StringEquals"
      variable = "iam:PassedToService"
      values   = ["ecs-tasks.amazonaws.com"]
    }
  }
}

resource "aws_iam_role_policy" "deploy" {
  name   = "deploy"
  role   = aws_iam_role.deploy.id
  policy = data.aws_iam_policy_document.deploy.json
}
