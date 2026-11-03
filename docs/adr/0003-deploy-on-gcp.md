# ADR 0003: Deploy on GCP in Singapore: the bot on a GCE VM, the runtime on Cloud Run

- Status: Accepted (Terraform on `feat/gcp-deploy`; `plan` only, not applied)
- Date: 2026-11-03 (revised the same day: from both services on the VM to the hybrid)
- Details: [infra/gcp/README.md](../../infra/gcp/README.md),
  [previous AWS design](../../infra/aws/README.md)

## Context

TatarBot runs two independent containers:
- **The Rust Discord bot:** gateway WebSocket plus UDP voice, outbound only, and it must be
  connected around the clock.
- **The Python Agent Runtime:** HTTP, called only by the bot, idle most of the time.

State lives outside them: Supabase Postgres (ap-southeast-1) for library metadata and
object storage for library audio. Until this change, conversation history was in the
runtime's memory.

The AWS stack (ECS on one t4g.micro, `infra/aws`) was applied on 2026-10-19 with a
3-hours-a-day schedule. The decision is to move to GCP, in Singapore, close to Supabase,
the Qwen endpoint and the users. The first GCP design put both containers on one VM. The
runtime's load is bursty and mostly idle, which suits a scale-to-zero service better.

## Decision

1. **The bot on one e2-micro VM, 24/7,** on Container-Optimized OS, as a systemd unit that
   the startup script writes on every boot. It deploys over IAP SSH with `deploy.sh`, which
   rolls back if the bot doesn't connect.
2. **The runtime on Cloud Run:** min 0, max 1 instance, request-based billing, 1 vCPU and
   512 MiB, 150 s request timeout, in the same region.
3. **Private runtime, IAM-authenticated.** Ingress is internal: only VPC networks of this
   project, which includes the VM even with an external IP, so there's no connector or NAT.
   `roles/run.invoker` goes to the VM's service account only. The runtime's
   `AGENT_API_KEY` stays as a second check.
4. **ID tokens minted by the host, not the bot.** A systemd timer fetches an
   audience-bound Google ID token from the metadata server every 10 minutes and writes it
   to a tmpfs file that only the bot's UID can read. The bot re-reads it per request and
   sends it as `X-Serverless-Authorization`, which leaves `Authorization` for the API key.
   The bot container stays blocked from the metadata server, so it never holds a
   service-account credential.
5. **Conversation history in Postgres** (`CONVERSATION_STORE=postgres`, schema `chat`,
   30-day retention) behind the existing `ConversationStore` port, so scale-to-zero and
   deploys lose nothing. A database outage degrades to no history instead of failing chat.
6. **Unchanged from the first GCP design:**
   - Secret Manager, with per-secret access for each consumer;
   - GCS through its S3 API with an HMAC key;
   - Artifact Registry with immutable `sha-` tags;
   - Workload Identity Federation for deploys;
   - `linux/amd64` images.
7. **Standard network tier** for the VM. Its first 200 GiB of internet egress a month are
   free, which covers the voice traffic.

## Alternatives considered

* **Both services on the VM** (the first version of this ADR): no cold starts and the same
  VM price. But the runtime shares the e2-micro's 1 GiB with the bot and COS, and every
  deploy or restart wipes in-memory history.
* **Runtime with `min_instances = 1`:** no cold starts, but about $13.80/month of idle
  instance time, more than the VM. Available by variable if cold starts prove too slow.
* **The bot on Cloud Run:** needs always-allocated CPU and a long-lived UDP session. It
  would cost several times the VM and fight Cloud Run's request model.
* **Unblocking the metadata server for the bot** so it fetches its own token: simpler, but
  the bot would then hold an access token for the VM's identity (secret reads, image pulls),
  not just a token that can only call the runtime.
* **`Authorization` for the ID token, dropping `AGENT_API_KEY`:** one credential fewer.
  The key costs nothing, keeps local and AWS setups working, and guards against an IAM
  misconfiguration.
* **Serverless VPC Access connector or Direct VPC egress for the runtime:** not needed.
  The runtime only calls public APIs, and the VM reaches it through internal ingress.
* **Redis or Firestore for history:** one more service. Supabase is already there, and the
  runtime already has a Postgres pool and migration runner.

## Consequences

* About **$9.40/month** with catalog prices, or about $13 if the external IP's free hours
  don't apply to this account. The VM is about 80% of it, and Cloud Run is about $0.50
  before any free tier.
* The first message after about 15 idle minutes pays a cold start. That's 1.3 s locally,
  estimated 2–5 s on Cloud Run, not measured there yet. The bot's 120 s timeout includes a
  15 s margin for it.
* The runtime needs a database (`tatarbot-runtime-database-url`) and the migrations before
  the first deploy. Cloud Run needs the secrets to have versions before the service is
  created, so `apply` runs in two phases.
* One more moving part on the VM (the token timer). If it fails, chat stops after about
  50 minutes while music keeps working.
* Conversation text now persists in Supabase for up to 30 days, as personal data.
* The bot still depends on GCS's S3 compatibility, unverified until the first live run. The
  HMAC key is still the one long-lived credential.
* CI builds and tests both amd64 and arm64 images until AWS is destroyed.

## Revisit when

* Cold starts annoy users: set `runtime_min_instances = 1`, or move the runtime back onto
  the VM.
* The runtime needs more than one instance: the per-conversation lock moves to the database
  (`pg_advisory_xact_lock` on the conversation).
* The knowledge base is enabled: request-based billing throttles background indexing, so
  use `RAG_STORE=postgres` with ingestion in a worker or job.
* The VM runs out of memory: move to e2-small.
