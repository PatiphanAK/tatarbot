# Run the whole TatarBot stack locally: Agent Runtime + Discord bot.

SHELL := /bin/bash
.DEFAULT_GOAL := help
.PHONY: help up check images compose-up compose-down integration

help: ## Show available targets
	@grep -E '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-13s\033[0m %s\n", $$1, $$2}'

up: ## Start the Agent Runtime, wait until it is healthy, then start the bot (Ctrl-C stops both)
	@$(MAKE) -s -C agent_runtime run & RUNTIME_PID=$$!; \
	trap 'kill $$RUNTIME_PID 2>/dev/null; pkill -P $$RUNTIME_PID 2>/dev/null' EXIT INT TERM; \
	for i in $$(seq 50); do curl -sf localhost:8080/health >/dev/null && break; sleep 0.2; done; \
	curl -sf localhost:8080/health >/dev/null || { echo "agent runtime failed to start"; exit 1; }; \
	curl -s localhost:8080/ready; echo; \
	$(MAKE) -s -C discord_bot run

check: ## Lint and test both services
	$(MAKE) -C agent_runtime check
	$(MAKE) -C discord_bot check

images: ## Build both container images
	docker compose build

compose-up: ## Run both services as separate containers (uses both .env files; Ctrl-C stops it)
	docker compose up --build

compose-down: ## Stop and remove the containers
	docker compose down

integration: ## Runtime image vs a mock OpenAI-compatible model (no Discord, GPU or AWS needed)
	./integration/run.sh
