# ingest-core

Generic asynchronous file ingestion (ADR 0004). TatarBot's music library is the first user.
The crate knows nothing about Discord: `cargo tree -p ingest-core` has no serenity or
songbird, and the schema test refuses the word.

## Contract

```text
POST /v1/jobs            producer = the verified `email` of its Google ID token
  { idempotency_key, tenant, actor, pipeline, params,
    source: { kind: "https_url", uri, declared_size } }
  → 201 { job_id, state, ... } | 200 (same key: the existing job) | 400 policy_denied
    | 403 not_a_producer / producer_disabled | 429 too_many_active / tenant_rate_limited
    | 503 paused
GET  /v1/jobs/{id}       → { job_id, state, attempts, result, error_code, error_detail, ... }
POST /v1/jobs/{id}/cancel → 202
POST /internal/process               Cloud Tasks (tasks-invoker identity only)
POST /internal/maintenance/reap      Cloud Scheduler: recover dead attempts, create tasks
POST /internal/maintenance/consumers Cloud Scheduler: consumer housekeeping (orphan sweep)
POST /internal/maintenance/budget    budget Pub/Sub push: pause all ingestion at 100 %
```

Every route verifies the Google ID token in `Authorization` itself: RS256 against Google's
keys, audience, issuer, expiry, `email_verified`. Cloud Run IAM in front only decides who may
reach the service. A token whose signature was stripped (what Cloud Run does to
`X-Serverless-Authorization`) is rejected. Bodies are parsed only after authentication.

## Extension points (`ports.rs`)

* **`Pipeline`:** input file → output file + metadata (`version`, `validate_params`, `run`).
* **`Consumer`** (optional, per producer, compiled into the worker): `on_fetched` (skip or
  reuse), `output_key`, and `finalize_succeeded` / `finalize_failed` / `finalize_cancelled`,
  which run **inside** the transaction that changes the job's state. So domain effects happen
  exactly once. A producer without a consumer reads `GET /v1/jobs/{id}` and
  `ingest.job_events` (event mode).
* **`ArtifactStore`** (create-only writes) and **`TaskQueue`** (at-least-once delivery):
  GCS and Cloud Tasks clients in `gcp.rs`; `LocalArtifacts` and `InlineQueue` for
  development.

## Guarantees and failure handling

* **Idempotent admission:** `(producer, idempotency_key)` is unique; admission limits are
  checked under a per-tenant advisory lock.
* **Lease fencing:** one attempt per delivery holds a 12-minute lease with a random owner.
  Terminal transitions re-check the owner under `FOR UPDATE`; a stale attempt's commit is
  discarded.
* **Failure classes:**
  * Permanent: recorded and acknowledged (2xx).
  * Transient: lease released, 503, so Cloud Tasks retries.
  * Resource-limit: retried once, then `processing_limit`.
  * The last attempt ends `retries_exhausted`.
* **Cancellation:** a queued job is cancelled at once. A processing one is flagged, checked
  after fetch, after the pipeline and in the commit fence.
* **Kill switch** (`ingest.switches`): paused jobs stay queued without spending attempts;
  the reaper re-enqueues them on resume.
* **Reaper:**
  * requeues expired leases (or fails them when attempts are used up);
  * times out jobs stuck queued for an hour;
  * creates missing tasks;
  * purges finished jobs after 30 days.

## Tests

`cargo test -p ingest-core` (unit). `DATABASE_URL=… cargo test -p ingest-core -- --ignored`
runs the engine and HTTP tests against real Postgres (`make -C discord_bot test-db`).
