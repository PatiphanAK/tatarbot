# Dedicated VPC with two public subnets and no NAT gateway.
#
# The EC2 instance (launch_type = "EC2") or each Fargate task gets a public IPv4 address
# only for outbound traffic (Discord gateway and voice, the inference provider, ECR,
# CloudWatch, SSM). Nothing is reachable from the internet: no security group here has an
# ingress rule from outside the VPC.
# Private subnets + NAT gateway (or VPC endpoints) is the stricter alternative; it costs
# more per month than the rest of this stack (see README).

locals {
  azs     = slice(data.aws_availability_zones.available.names, 0, 2)
  ec2     = var.launch_type == "EC2"
  fargate = !local.ec2

  namespace = "${var.project}.internal"
}

resource "aws_vpc" "main" {
  cidr_block           = var.vpc_cidr
  enable_dns_support   = true
  enable_dns_hostnames = true # required for Cloud Map private DNS

  tags = { Name = var.project }
}

resource "aws_internet_gateway" "main" {
  vpc_id = aws_vpc.main.id
  tags   = { Name = var.project }
}

resource "aws_subnet" "public" {
  count = length(local.azs)

  vpc_id                  = aws_vpc.main.id
  availability_zone       = local.azs[count.index]
  cidr_block              = cidrsubnet(var.vpc_cidr, 8, count.index)
  map_public_ip_on_launch = false # the launch template / ECS assigns public IPs

  tags = { Name = "${var.project}-public-${local.azs[count.index]}" }
}

resource "aws_route_table" "public" {
  vpc_id = aws_vpc.main.id

  route {
    cidr_block = "0.0.0.0/0"
    gateway_id = aws_internet_gateway.main.id
  }

  tags = { Name = "${var.project}-public" }
}

resource "aws_route_table_association" "public" {
  count = length(aws_subnet.public)

  subnet_id      = aws_subnet.public[count.index].id
  route_table_id = aws_route_table.public.id
}

# Strip the default security group so nothing can accidentally use its allow-all rules.
resource "aws_default_security_group" "default" {
  vpc_id = aws_vpc.main.id
}

# ---- EC2: one security group for the container instance. Both tasks use bridge
# networking, so they share it. The bot reaches the runtime's host port over the Docker
# bridge on the instance itself; no rule admits it from the network.

resource "aws_security_group" "ecs_host" {
  count = local.ec2 ? 1 : 0

  name        = "${var.project}-ecs-host"
  description = "ECS container instance: no inbound, outbound only"
  vpc_id      = aws_vpc.main.id
}

resource "aws_vpc_security_group_egress_rule" "ecs_host_all" {
  count = local.ec2 ? 1 : 0

  security_group_id = aws_security_group.ecs_host[0].id
  description       = "Discord gateway and voice, inference provider, ECR, CloudWatch, SSM"
  ip_protocol       = "-1"
  cidr_ipv4         = "0.0.0.0/0"
}

# ---- Fargate: one security group per task (awsvpc) and Cloud Map for bot -> runtime.

resource "aws_security_group" "discord" {
  count = local.fargate ? 1 : 0

  name        = "${var.project}-discord"
  description = "Discord bot: no inbound, outbound only"
  vpc_id      = aws_vpc.main.id
}

resource "aws_vpc_security_group_egress_rule" "discord_all" {
  count = local.fargate ? 1 : 0

  security_group_id = aws_security_group.discord[0].id
  description       = "Discord gateway, runtime, ECR, CloudWatch, SSM"
  ip_protocol       = "-1"
  cidr_ipv4         = "0.0.0.0/0"
}

resource "aws_security_group" "runtime" {
  count = local.fargate ? 1 : 0

  name        = "${var.project}-runtime"
  description = "Agent Runtime: inbound only from the bot"
  vpc_id      = aws_vpc.main.id
}

resource "aws_vpc_security_group_ingress_rule" "runtime_from_discord" {
  count = local.fargate ? 1 : 0

  security_group_id            = aws_security_group.runtime[0].id
  description                  = "Bot -> runtime HTTP"
  ip_protocol                  = "tcp"
  from_port                    = var.runtime_port
  to_port                      = var.runtime_port
  referenced_security_group_id = aws_security_group.discord[0].id
}

resource "aws_vpc_security_group_egress_rule" "runtime_all" {
  count = local.fargate ? 1 : 0

  security_group_id = aws_security_group.runtime[0].id
  description       = "Inference provider, ECR, CloudWatch, SSM"
  ip_protocol       = "-1"
  cidr_ipv4         = "0.0.0.0/0"
}

# Private DNS for service discovery: runtime.<project>.internal resolves to the runtime
# task's private IP inside this VPC only.
resource "aws_service_discovery_private_dns_namespace" "main" {
  count = local.fargate ? 1 : 0

  name        = local.namespace
  description = "Private service discovery for ${var.project}"
  vpc         = aws_vpc.main.id
}

resource "aws_service_discovery_service" "runtime" {
  count = local.fargate ? 1 : 0

  name = "runtime"

  dns_config {
    namespace_id   = aws_service_discovery_private_dns_namespace.main[0].id
    routing_policy = "MULTIVALUE"

    dns_records {
      type = "A"
      ttl  = 10
    }
  }

  # ECS reports task health (from the container health check) to Cloud Map.
  health_check_custom_config {}
}
