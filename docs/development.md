# Development and testing

How to set up a machine, run the stack locally, and which tests cover what. Per-service
details (every variable, every target) are in each service's README; run `make help` in
any directory with a Makefile to list its targets.

## Prerequisites

| Tool | Version | For |
|---|---|---|
| Rust | 1.99 (CI and `discord_bot/Dockerfile` pin it), edition 2024 | `discord_bot` |
| uv | 0.8.22 (CI and `agent_runtime/Dockerfile` pin it) | `agent_runtime` (Python ≥ 3.12, installed by uv) |
| Docker + Compose | recent | local Postgres/RustFS, images, integration tests |
| ffmpeg / ffprobe | any recent | the bot's library tests on the host (the image bundles its own) |
| sqlx-cli | – | only to refresh the bot's `.sqlx` cache after changing SQL |
| Terraform | see `infra/aws/versions.tf` | only for infrastructure work |

## First run

```sh
make -C agent_runtime install env   # .venv + .env with a generated AGENT_API_KEY
make -C discord_bot env             # .env; set DISCORD_TOKEN and the same AGENT_API_KEY
# point agent_runtime/.env at a model (MODEL_BASE_URL, MODEL_NAME, MODEL_API_KEY):
#   a local vLLM (model-inference/), Runpod, or Alibaba Model Studio
make up                             # runtime on :8080, waits for /health, then the bot
```

No model handy: `make -C discord_bot dev` runs the bot against an echo mock of the runtime.
`make compose-up` runs both services as containers (see [`compose.yaml`](../compose.yaml)).

## Local backing services

[`compose.yaml`](../compose.yaml) provides throwaway stand-ins, published on localhost only:

| Service | Image | Port | Used by |
|---|---|---|---|
| `postgres` | `pgvector/pgvector:0.8.1-pg17-trixie` | `127.0.0.1:55432` (`POSTGRES_PORT`) | bot library; runtime `RAG_STORE=postgres` and `make -C agent_runtime test-db` |
| `rustfs` | `rustfs/rustfs:1.0.0` (S3-compatible) | `127.0.0.1:59000` (`RUSTFS_PORT`) | bot library |

```sh
make -C discord_bot library-up   # postgres + rustfs
make -C agent_runtime db-up      # postgres only
make -C agent_runtime migrate    # apply the rag schema to DATABASE_URL in agent_runtime/.env
```

A `postgres` container created before the switch to the pgvector image keeps running the
old image until it is recreated (`docker compose up -d postgres`). Without pgvector the
runtime's migrations fail on `CREATE EXTENSION vector`. Recreating keeps the data volume
but changes the base image (Alpine to Debian), and a different C library can change text
collation; take a dump first if the data matters.

There's no local SQS. To exercise the upload worker end to end, point `RAG_SQS_QUEUE_URL`
and `AWS_ENDPOINT_URL_SQS` at an SQS-compatible service of your choice, or call
`SqsWorker.handle` with an S3 event as the tests do.

## Tests

| Suite | Command | Needs | Covers |
|---|---|---|---|
| Runtime unit | `make -C agent_runtime test` | nothing | everything not marked `db`; database tests are skipped |
| Runtime with database | `make -C agent_runtime test-db` | Docker (pgvector Postgres) | adds migrations, `PostgresVectorStore`, ingestion lifecycle, hybrid retrieval ([tests README](../agent_runtime/tests/README.md)) |
| Bot unit | `make -C discord_bot check` | nothing | fmt, clippy `-D warnings`, unit tests |
| Bot library | `make -C discord_bot test-db` | Docker, ffmpeg | Postgres races and search, RustFS, ffmpeg hardening, end to end |
| Bot live stations | `cargo test -- --ignored stations_decode icy_live` (in `discord_bot/`) | internet | real radio streams |
| Integration | `make integration` | Docker | runtime image against a mock OpenAI-compatible model, with the model up and down ([integration/README.md](../integration/README.md)) |
| Agent eval | `make -C agent_runtime eval` | a model in `agent_runtime/.env` (costs money) | the real model's decisions, tool calls, format and language, logged to MLflow ([evals/README.md](../agent_runtime/evals/README.md)) |
| Inference smoke | see [model-inference/README.md](../model-inference/README.md#smoke-test) | an endpoint or the mock | OpenAI compatibility of an endpoint |
| Everything quick | `make check` (repo root) | nothing | runtime lint + tests, bot check |

Database tests each create and drop their own database, so they can run against a
development server without touching its data.

## CI

GitHub Actions run per changed path; the table is in
[infra/aws/README.md](../infra/aws/README.md#cicd-github-actions). CI uses no secrets.
Runtime CI runs the database tests against a pgvector service container. Deploys are
manual workflows from `main` only.

## Conventions

* **Architecture:** ports and adapters in both services. Domain and application code
  imports no framework, HTTP client or database driver; composition roots (`main.rs`,
  `library.rs`; `main.py`, `worker.py`) are the only places that pick adapters.
* **Migrations** are append-only in both services; never edit an applied file
  ([database.md](architecture/database.md)). After changing bot SQL, run
  `make -C discord_bot sqlx-prepare` and commit `.sqlx/`.
* **Configuration** comes from environment variables only; each service validates it at
  startup and fails with a named variable. New variables go into the service's
  `.env.example` and README table.
* **Formatting/lint:** `make fmt` / `make lint` in `agent_runtime` (ruff, line length 100);
  `cargo fmt` and clippy in `discord_bot`.
* **Docs** live next to the code they describe; see the [documentation map](README.md).
