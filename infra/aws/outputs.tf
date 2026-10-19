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
  description = "AGENT_BASE_URL the bot uses: the Docker bridge on the instance (EC2) or Cloud Map DNS inside the VPC (Fargate)."
  value       = local.runtime_url
}

output "ecs_instance_shell" {
  description = "EC2 only: how to open a shell on the container instance (Session Manager, no SSH)."
  value = local.ec2 ? join(" ", [
    "aws ssm start-session --region ${var.region} --target",
    "$(aws autoscaling describe-auto-scaling-groups --region ${var.region} --auto-scaling-group-names ${aws_autoscaling_group.ecs[0].name}",
    "--query 'AutoScalingGroups[0].Instances[0].InstanceId' --output text)",
  ]) : null
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
