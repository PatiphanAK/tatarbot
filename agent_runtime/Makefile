# TatarBot Agent Runtime – local development tasks.

SHELL    := /bin/bash
ENV_FILE ?= .env
LOAD_ENV := set -a; [ -f $(ENV_FILE) ] && . ./$(ENV_FILE); set +a
UVICORN  := uv run uvicorn --factory agent_runtime.main:create_app
IMAGE    ?= tatarbot-agent-runtime:local

.DEFAULT_GOAL := help
.PHONY: help install env run dev test lint fmt check smoke image image-run

help: ## Show available targets
	@grep -E '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-10s\033[0m %s\n", $$1, $$2}'

install: ## Install dependencies into .venv
	uv sync

env: ## Create .env from .env.example with a generated AGENT_API_KEY
	@if [ -f $(ENV_FILE) ]; then echo "$(ENV_FILE) already exists"; else \
		KEY=$$(python3 -c "import secrets; print(secrets.token_urlsafe(32))"); \
		sed "s|^AGENT_API_KEY=.*|AGENT_API_KEY=$$KEY|" .env.example > $(ENV_FILE); \
		chmod 600 $(ENV_FILE); \
		echo "created $(ENV_FILE); use the same AGENT_API_KEY in discord_bot/.env"; fi

run: ## Run the runtime with .env
	@$(LOAD_ENV); $(UVICORN) --host $${HOST:-127.0.0.1} --port $${PORT:-8080}

dev: ## Run with auto-reload
	@$(LOAD_ENV); $(UVICORN) --reload --host $${HOST:-127.0.0.1} --port $${PORT:-8080}

test: ## Run tests
	uv run pytest -q

lint: ## Lint and check formatting
	uv run ruff check .
	uv run ruff format --check .

fmt: ## Format and auto-fix
	uv run ruff format .
	uv run ruff check --fix .

check: lint test ## Lint + tests

smoke: ## Call a running runtime: /health, /ready, /v1/chat
	@$(LOAD_ENV); URL=http://$${HOST:-127.0.0.1}:$${PORT:-8080}; \
	echo "health: $$(curl -s $$URL/health)"; \
	echo "ready:  $$(curl -s $$URL/ready)"; \
	curl -s -XPOST $$URL/v1/chat -H "Authorization: Bearer $$AGENT_API_KEY" \
		-H 'Content-Type: application/json' \
		-d '{"message":"Say hello in one short sentence.","conversation_id":"smoke:test"}'; echo

image: ## Build the container image ($(IMAGE))
	docker build -t $(IMAGE) .

image-run: ## Run the image with .env, published on 127.0.0.1:PORT (default 8080) only
	docker run --rm --init --env-file $(ENV_FILE) -e UVICORN_HOST=0.0.0.0 -e UVICORN_PORT=8080 \
		-p 127.0.0.1:$${PORT:-8080}:8080 $(IMAGE)
