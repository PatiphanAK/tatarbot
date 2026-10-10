# TatarBot Discord adapter – local development tasks.
# Variables from .env are loaded into every recipe that needs them.

SHELL       := /bin/bash
ENV_FILE    ?= .env
MOCK_PORT   ?= 8080
IMAGE       ?= tatarbot-discord:local
LOAD_ENV    := set -a; [ -f $(ENV_FILE) ] && . ./$(ENV_FILE); set +a
# Library backends from ../compose.yaml (`make library-up`); throwaway local credentials.
DEV_DB      := postgres://tatarbot:tatarbot-local@127.0.0.1:$${POSTGRES_PORT:-55432}/tatarbot
DEV_S3      := MUSIC_S3_ENDPOINT=http://127.0.0.1:$${RUSTFS_PORT:-59000} MUSIC_S3_BUCKET=tatarbot-test \
               AWS_ACCESS_KEY_ID=tatarbot-local AWS_SECRET_ACCESS_KEY=tatarbot-local-secret \
               AWS_REGION=us-east-1

.DEFAULT_GOAL := help

.PHONY: help env run run-release dev mock-agent build release test lint fmt fmt-check check ci clean image image-run \
	library-up library-down test-db sqlx-prepare sqlx-check

help: ## Show available targets
	@grep -E '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}'

env: ## Create .env from .env.example if missing
	@if [ -f $(ENV_FILE) ]; then echo "$(ENV_FILE) already exists"; \
	else cp .env.example $(ENV_FILE) && echo "created $(ENV_FILE) – set DISCORD_TOKEN"; fi

run: _require-token ## Run the bot (debug build) with .env
	@$(LOAD_ENV); cargo run

run-release: _require-token ## Run the bot (release build) with .env
	@$(LOAD_ENV); cargo run --release

dev: _require-token ## Bot + echo mock Agent Runtime; Ctrl-C stops both
	@$(LOAD_ENV); MOCK_AGENT_API_KEY="$$AGENT_API_KEY" python3 scripts/mock_agent.py $(MOCK_PORT) & \
	MOCK_PID=$$!; trap 'kill $$MOCK_PID 2>/dev/null' EXIT INT TERM; \
	$(LOAD_ENV); AGENT_BASE_URL=http://127.0.0.1:$(MOCK_PORT) cargo run

mock-agent: ## Run only the echo mock Agent Runtime on MOCK_PORT
	@python3 scripts/mock_agent.py $(MOCK_PORT)

build: ## Debug build
	cargo build

release: ## Optimised build (target/release/tatarbot-discord)
	cargo build --release

test: ## Run unit tests
	cargo test

lint: ## Run clippy, warnings as errors
	cargo clippy --all-targets -- -D warnings

fmt: ## Format code
	cargo fmt

fmt-check: ## Check formatting
	cargo fmt --check

check: fmt-check lint test ## Format check, lint and test

ci: check ## Alias for check

library-up: ## Start local Postgres + RustFS for the music library (compose)
	docker compose -f ../compose.yaml up -d --wait postgres rustfs
	@for i in $$(seq 30); do \
		curl -s -o /dev/null http://127.0.0.1:$${RUSTFS_PORT:-59000}/ && exit 0; sleep 1; done; \
		echo "RustFS did not come up"; exit 1

library-down: ## Stop local Postgres + RustFS (data volumes are kept)
	docker compose -f ../compose.yaml stop postgres rustfs

test-db: library-up ## Integration tests: Postgres races, RustFS, ffmpeg, end to end
	DATABASE_URL=$(DEV_DB) $(DEV_S3) cargo test -- --ignored pg_ s3_ ffmpeg_ library_end_to_end

sqlx-prepare: library-up ## Refresh the .sqlx query cache after changing SQL (commit it)
	DATABASE_URL=$(DEV_DB) cargo sqlx migrate run
	DATABASE_URL=$(DEV_DB) cargo sqlx prepare -- --all-targets

sqlx-check: library-up ## Fail if .sqlx is out of date with the queries
	DATABASE_URL=$(DEV_DB) cargo sqlx migrate run
	DATABASE_URL=$(DEV_DB) cargo sqlx prepare --check -- --all-targets

clean: ## Remove build artifacts
	cargo clean

image: ## Build the container image ($(IMAGE))
	docker build -t $(IMAGE) .

image-run: _require-token ## Run the image with .env (a runtime on this machine: AGENT_BASE_URL=http://host.docker.internal:8080)
	docker run --rm --env-file $(ENV_FILE) --add-host host.docker.internal:host-gateway $(IMAGE)

_require-token:
	@$(LOAD_ENV); if [ -z "$$DISCORD_TOKEN" ]; then \
		echo "DISCORD_TOKEN is not set. Run 'make env' and fill it in $(ENV_FILE)."; exit 1; fi
