output "ecr_repository_urls" {
  description = "Image repositories, one per service."
  value       = { for k, r in aws_ecr_repository.service : k => r.repository_url }
}

output "ecs_cluster" {
  value = aws_ecs_cluster.main.name
}

output "ecs_services" {
  value = { for k, s in local.ecs_services : k => s.name }
}

output "runtime_private_url" {
  description = "AGENT_BASE_URL the bot uses; resolvable only inside the VPC."
  value       = local.runtime_url
}

output "ssm_parameters_to_create" {
  description = "SecureString parameters to create with the AWS CLI before scaling up."
  value       = local.secret_names
}

output "github_actions_variables" {
  description = "Repository or environment variables for the deploy workflows."
  value = {
    AWS_REGION          = var.region
    AWS_DEPLOY_ROLE_ARN = aws_iam_role.deploy.arn
    ECS_CLUSTER         = aws_ecs_cluster.main.name
  }
}
