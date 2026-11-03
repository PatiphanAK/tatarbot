# Security and access control

A map of trust boundaries and controls, with links to where each is implemented and
described in detail. It covers what exists. Items marked *planned* are designed, not built.

## Trust boundaries

```mermaid
flowchart LR
    users([Discord users]) -->|"untrusted text,<br/>attachments, links"| bot
    subgraph gcp["GCP project (Terraform in infra/gcp, not applied)"]
        bot["discord_bot<br/>(GCE VM)"]
        rt["agent_runtime<br/>(Cloud Run, private)"]
        worker["ingestion worker<br/>(planned deployment)"]
    end
    bot -->|"ID token (run.invoker)<br/>+ bearer AGENT_API_KEY"| rt
    rt -->|"bearer MODEL_API_KEY"| model[(model provider)]
    rt -->|"API key"| tavily[(Tavily)]
    rt -->|"untrusted results"| model
    bot -->|"SSRF-guarded GET"| web[(arbitrary audio URLs)]
    bot --> pg[(Supabase Postgres)]
    rt --> pg
    worker --> pg
    bot -->|"HMAC key"| s3m[(GCS music bucket)]
    rt -->|"presigned POST"| s3r[(S3 RAG prefix)]
    s3r --> sqs[(SQS)] --> worker
```

| Input | Treated as | Controls |
|---|---|---|
| Chat messages | untrusted | input guardrails (prompt-injection patterns), output guardrails (prompt leaks, secret-looking strings): `agent_runtime/src/agent_runtime/domain/guardrails.py` |
| Web search results, knowledge excerpts | untrusted data, never instructions | marked as data in the system message; sources footer built from the actual results |
| Model tool calls (`play_music`) | untrusted | station must be one the bot sent; actions limited to play/play_track/skip/stop/queue; at most 2 per reply; links in track names dropped by the runtime and refused by the bot ([root README §6](../README.md#6-security)) |
| `/play` URLs | untrusted | SSRF-safe resolver (no private, loopback, link-local, CGNAT addresses), every redirect re-checked: `discord_bot/src/adapters/outbound/audio_http.rs` |
| `/upload` audio | untrusted | Discord CDN hosts only, size capped while streaming, ffprobe validation, hardened minimal ffmpeg build: `ffmpeg.rs`, `attachment_fetch.rs` |
| Knowledge uploads | untrusted content from a trusted caller | server-generated keys, S3-enforced size, type detected from bytes, text/Markdown only ([rag.md §7.2](architecture/rag.md#72-what-is-trusted)) |

## Access control

| Who | Can | Enforced by |
|---|---|---|
| Any member who can mention the bot | chat; control music through chat within the music rules | Discord; `MusicUseCase` |
| Member in the bot's voice channel | play, skip, stop, queue | `MusicUseCase` (`discord_bot/src/application/use_cases/music.rs`) |
| Role `MUSIC_UPLOAD_ROLE` (default `DJ`) or Manage Server | upload, delete others' tracks | `discord_bot` library commands |
| Holder of `AGENT_API_KEY` (the bot, operators) | every runtime `/v1/*` route, including knowledge uploads and deletes in **any** namespace | `http_api.py` bearer check; on GCP also Cloud Run IAM (below) |
| The VM's service account (GCP) | invoke the Cloud Run runtime (`roles/run.invoker` on that service); nobody else can reach it | Cloud Run IAM + internal ingress (`infra/gcp/cloudrun.tf`) |
| A chat turn | read knowledge from `global` and its own server's namespace only | namespaces derived from `conversation_id` on the server (`domain/ingestion.py`, `retrieval_namespaces`) |

The runtime has no per-user identity. Any future bot command that manages knowledge
documents must check Discord permissions itself before calling the runtime. Letting
ordinary members upload would also let them plant prompt injections that every chat in
their server retrieves.

## Secrets

* Never in the repository, images, Terraform variables or state. Locally they live in
  git-ignored `.env` files (the runtime's `make env` creates its file with mode 600; the
  bot's copies `.env.example` as is, so restrict it yourself). On GCP they live in Secret
  Manager, readable per secret by the consuming service's identity only: the VM's host
  writes the bot's to tmpfs at each start, Cloud Run injects the runtime's at instance start
  ([infra/gcp/README.md](../infra/gcp/README.md#identity-secrets-and-the-metadata-server)).
* The bot calls the runtime with a Google ID token that the host mints and refreshes; the
  bot container never holds a service-account credential (it can't reach the metadata
  server). The one long-lived credential is the music bucket's HMAC key (one bucket only).
  Deploys use Workload Identity Federation, no stored keys.
* The runtime sends `MODEL_API_KEY` to the embedding server only when the embedding URL is
  defaulted to the model's: a key never goes to a host it wasn't configured for
  (`config.py`, `_embedding_from_env`).
* Secret settings are excluded from `repr` (`config.py`, `field(repr=False)`); upstream
  error bodies are logged truncated and never returned to callers.

## Network

* No inbound internet access to either service. The VM's firewall admits only IAP's range
  on port 22 (OS Login). The runtime's Cloud Run ingress is internal (this project's VPC)
  and invocation requires IAM, granted to the VM's identity only.
* The bot container can't reach the metadata server (iptables `DOCKER-USER` rules; the unit
  refuses to start without them), so it never gets the VM's service account.
* TLS to Supabase is verified against its root CA (`sslmode=verify-full`).

## IAM (least privilege)

GCP (`infra/gcp`, not applied); the full table is in
[infra/gcp/README.md](../infra/gcp/README.md#identity-secrets-and-the-metadata-server).

| Identity | Permissions |
|---|---|
| `tatarbot-vm` (host only) | pull the bot image, read the bot's secrets, `run.invoker` on the runtime service, write logs and metrics |
| `tatarbot-runtime` (Cloud Run) | read the runtime's secrets |
| `tatarbot-music` (HMAC key) | object user on the music bucket |
| `tatarbot-deploy` (GitHub, `production` environment only) | push both images; OS Login with sudo and IAP on the VM; `run.developer` on the runtime service and actAs its identity |
| Upload and worker identities | *planned* ([rag.md §7.6](architecture/rag.md#76-deployment-requirements-not-built-yet)) |

## Containers

Both images run as non-root users with no secrets baked in. The bot's image is distroless
(no shell) with a read-only root filesystem on the VM (`/tmp` is a scratch volume, the ID
token a read-only mount). The
runtime's is `python:3.12-slim` with a virtualenv.

