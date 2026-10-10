# Dedicated VPC with two public subnets and no NAT gateway.
#
# Tasks get a public IPv4 address only for outbound traffic (Discord, the inference
# provider, ECR, CloudWatch, SSM). Nothing is reachable from the internet: the bot's
# security group has no ingress rules and the runtime's only admits the bot on its port.
# Private subnets + NAT gateway (or VPC endpoints) is the stricter alternative; it costs
# more per month than the rest of this stack (see README).

locals {
  azs = slice(data.aws_availability_zones.available.names, 0, 2)
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
  map_public_ip_on_launch = false # ECS assigns public IPs per service

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

resource "aws_security_group" "discord" {
  name        = "${var.project}-discord"
  description = "Discord bot: no inbound, outbound only"
  vpc_id      = aws_vpc.main.id
}

resource "aws_vpc_security_group_egress_rule" "discord_all" {
  security_group_id = aws_security_group.discord.id
  description       = "Discord gateway, runtime, ECR, CloudWatch, SSM"
  ip_protocol       = "-1"
  cidr_ipv4         = "0.0.0.0/0"
}

resource "aws_security_group" "runtime" {
  name        = "${var.project}-runtime"
  description = "Agent Runtime: inbound only from the bot"
  vpc_id      = aws_vpc.main.id
}

resource "aws_vpc_security_group_ingress_rule" "runtime_from_discord" {
  security_group_id            = aws_security_group.runtime.id
  description                  = "Bot -> runtime HTTP"
  ip_protocol                  = "tcp"
  from_port                    = var.runtime_port
  to_port                      = var.runtime_port
  referenced_security_group_id = aws_security_group.discord.id
}

resource "aws_vpc_security_group_egress_rule" "runtime_all" {
  security_group_id = aws_security_group.runtime.id
  description       = "Inference provider, ECR, CloudWatch, SSM"
  ip_protocol       = "-1"
  cidr_ipv4         = "0.0.0.0/0"
}

# Private DNS for service discovery: runtime.<project>.internal resolves to the runtime
# task's private IP inside this VPC only.
resource "aws_service_discovery_private_dns_namespace" "main" {
  name        = "${var.project}.internal"
  description = "Private service discovery for ${var.project}"
  vpc         = aws_vpc.main.id
}

resource "aws_service_discovery_service" "runtime" {
  name = "runtime"

  dns_config {
    namespace_id   = aws_service_discovery_private_dns_namespace.main.id
    routing_policy = "MULTIVALUE"

    dns_records {
      type = "A"
      ttl  = 10
    }
  }

  # ECS reports task health (from the container health check) to Cloud Map.
  health_check_custom_config {}
}
