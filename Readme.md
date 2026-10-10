# Tatarbot

```text
Discord ──▶ discord_bot/ (Rust) ──POST /v1/chat──▶ agent_runtime/ (Python) ──▶ vLLM / Qwen
```

* `discord_bot/` is the Discord adapter. It only knows the Agent Runtime's `/v1/chat` contract.
* `agent_runtime/` handles conversation history, the system prompt and the model port. It
  only knows the inference backend.

```sh
make up      # start the runtime on :8080, then the bot
make check   # lint and test both
```

## Containers

Each service has its own image and can be built, run, stopped and tested on its own.

| | Agent Runtime | Discord bot |
|---|---|---|
| Image | `agent_runtime/Dockerfile` (Python 3.12 slim, uv, uid 10001) | `discord_bot/Dockerfile` (Rust build → distroless, uid 65532) |
| Build | `make -C agent_runtime image` | `make -C discord_bot image` |
| Run alone | `make -C agent_runtime image-run` (127.0.0.1:8080 only) | `make -C discord_bot image-run` |
| Stop | Ctrl-C / SIGTERM drains in-flight requests | Ctrl-C / SIGTERM closes the gateway connection |
| Health | `GET /health` (image `HEALTHCHECK`) | none (no HTTP port); the process exits on fatal errors |

Secrets are read from the environment at start (`.env` locally, SSM in ECS); images contain
none. The bot running alone against a runtime on your machine needs
`AGENT_BASE_URL=http://host.docker.internal:8080` in `discord_bot/.env`.

### Whole stack with Docker Compose

```sh
make compose-up      # docker compose up --build: both services, separate containers
make compose-down
docker compose up --build agent_runtime     # runtime only, inside the Compose network
```

The services share the default Compose network; the bot calls `http://agent_runtime:8080`.
No port is published to the host. The bot starts after the runtime's health check passes;
that ordering is a local convenience only. In ECS the services are independent and the bot
handles an unavailable runtime per message. To reach a model on your machine from the
runtime container, set `MODEL_BASE_URL=http://host.docker.internal:8000/v1`.

### Integration tests (no Discord, GPU or AWS)

```sh
make integration     # ./integration/run.sh
```

Builds the runtime image and runs it against `integration/mock_openai.py`, a mock
OpenAI-compatible backend, on a private Compose network. It checks auth, history, reasoning
stripping and every failure mapping, then stops the mock and checks the "model down" path.

## Failure behaviour

| What fails | Runtime returns | Discord user sees |
|---|---|---|
| Runtime unreachable or not yet started | – | "⚠️ I can't reach my brain right now…" |
| Wrong `AGENT_API_KEY` between bot and runtime | 401 | same as above; the bot logs an operator error |
| Model unreachable, 5xx, 429 | 503 | same as above |
| Model rejects the request (4xx, e.g. wrong key/model, context too long) or invalid reply | 502 | same as above |
| Model slower than `MODEL_TIMEOUT_SECONDS` (100 s) | 504 | "⌛ That took too long to answer…" |
| Bot slower than `AGENT_TIMEOUT_SECS` (120 s) waiting for the runtime | – | same as above |

Failed turns are not stored in history. Upstream bodies, URLs and keys never reach the user.

## CI/CD (GitHub Actions)

| Workflow | Runs when | Does |
|---|---|---|
| `ci-runtime.yml` | `agent_runtime/**`, `integration/**`, `compose.yaml` change | ruff, pytest, image build, integration tests |
| `ci-discord.yml` | `discord_bot/**`, `compose.yaml` change | fmt, clippy, tests, image build, startup and SIGTERM smoke tests |
| `ci-repo.yml` | `.github/**`, compose files, `infra/**` change | actionlint, `docker compose config`, `terraform fmt` + `validate` (offline) |
| `deploy-runtime.yml` / `deploy-discord.yml` | manual only | via `_deploy-ecs.yml`: push `sha-<commit>` image to ECR (skipped if it exists), new task-definition revision, rolling ECS update |

CI uses `pull_request` (not `pull_request_target`), read-only tokens and no secrets, so
fork PRs run safely. Deploys run in the `production` GitHub environment and assume an AWS
role through OIDC that trusts only that environment of this repository. They refuse
commits that are not on `main`, and fail with a clear message until the AWS variables
are set. See `infra/aws/README.md` for the ECS setup, and
`docs/architecture/cloud-inference.md` for the design.
