# TatarBot model inference: local vLLM server and endpoint smoke tests.

SHELL    := /bin/bash
ENV_FILE ?= local/.env
# `smoke` tests whatever the runtime is configured to call.
RUNTIME_ENV ?= ../agent_runtime/.env
COMPOSE  := docker compose -f local/compose.yaml --env-file $(ENV_FILE)
NEED_ENV  = @[ -f $(ENV_FILE) ] || { echo "$(ENV_FILE) missing; run 'make env' first"; exit 1; }

.DEFAULT_GOAL := help
.PHONY: help env serve stop logs clean smoke smoke-local check

help: ## Show available targets
	@grep -E '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}'

env: ## Create local/.env from local/.env.example with a generated VLLM_API_KEY
	@if [ -f $(ENV_FILE) ]; then echo "$(ENV_FILE) already exists"; else \
		KEY=$$(python3 -c "import secrets; print(secrets.token_urlsafe(32))"); \
		sed "s|^VLLM_API_KEY=.*|VLLM_API_KEY=$$KEY|" local/.env.example > $(ENV_FILE); \
		chmod 600 $(ENV_FILE); \
		echo "created $(ENV_FILE); use the same key as MODEL_API_KEY in agent_runtime/.env"; fi

serve: ## Start local vLLM on the GPU and wait until it is healthy (first run downloads weights)
	$(NEED_ENV)
	$(COMPOSE) up --detach --wait vllm

stop: ## Stop local vLLM (keeps downloaded weights)
	$(NEED_ENV)
	$(COMPOSE) down

logs: ## Follow local vLLM logs
	$(NEED_ENV)
	$(COMPOSE) logs --follow vllm

clean: ## Stop local vLLM and delete the downloaded weights volume
	$(NEED_ENV)
	$(COMPOSE) down --volumes

smoke-local: ## Smoke-test the local vLLM server
	$(NEED_ENV)
	@set -a; . ./$(ENV_FILE); set +a; \
	MODEL_BASE_URL=http://127.0.0.1:$${VLLM_PORT:-8000}/v1 MODEL_NAME=$$SERVED_MODEL_NAME \
	MODEL_API_KEY=$$VLLM_API_KEY python3 scripts/smoke.py

smoke: ## Smoke-test the endpoint agent_runtime/.env points at (e.g. Runpod)
	@set -a; [ -f $(RUNTIME_ENV) ] && . $(RUNTIME_ENV); set +a; python3 scripts/smoke.py

check: ## Validate the Compose file and the smoke script (no GPU needed)
	VLLM_API_KEY=check docker compose -f local/compose.yaml config -q
	python3 -m py_compile scripts/smoke.py
