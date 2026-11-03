-- Generic ingestion core (ADR 0004). Owned by ingest-core; applied by an operator with
-- `ingest-worker migrate` (never on startup). Nothing here knows about any particular
-- producer: tenants and actors are opaque strings.

CREATE TABLE IF NOT EXISTS ingest.producers (
    id                       TEXT    PRIMARY KEY CHECK (id ~ '^[a-z][a-z0-9-]{1,39}$'),
    -- The verified `email` claim of the producer's Google ID token.
    service_account          TEXT    NOT NULL UNIQUE CHECK (service_account <> ''),
    -- Tenants this producer may act for: every tenant must start with this prefix.
    tenant_prefix            TEXT    NOT NULL CHECK (tenant_prefix <> ''),
    allowed_pipelines        TEXT[]  NOT NULL,
    allowed_source_kinds     TEXT[]  NOT NULL,
    -- Exact host names an https_url source may point at.
    source_url_hosts         TEXT[]  NOT NULL DEFAULT '{}',
    max_source_bytes         BIGINT  NOT NULL CHECK (max_source_bytes > 0),
    max_active_per_actor     INTEGER NOT NULL DEFAULT 3 CHECK (max_active_per_actor > 0),
    max_jobs_per_tenant_hour INTEGER NOT NULL DEFAULT 20 CHECK (max_jobs_per_tenant_hour > 0),
    enabled                  BOOLEAN NOT NULL DEFAULT true,
    updated_at               TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS ingest.jobs (
    id                 UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    producer_id        TEXT        NOT NULL REFERENCES ingest.producers (id),
    idempotency_key    TEXT        NOT NULL CHECK (char_length(idempotency_key) BETWEEN 1 AND 200),
    tenant             TEXT        NOT NULL CHECK (char_length(tenant) BETWEEN 1 AND 200),
    actor              TEXT        NOT NULL CHECK (char_length(actor) BETWEEN 1 AND 200),
    pipeline           TEXT        NOT NULL CHECK (char_length(pipeline) BETWEEN 1 AND 100),
    params             JSONB       NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(params) = 'object'),
    source_kind        TEXT        NOT NULL CHECK (source_kind IN ('https_url', 'gcs_object')),
    source_uri         TEXT        NOT NULL CHECK (char_length(source_uri) BETWEEN 1 AND 4000),
    declared_size      BIGINT      CHECK (declared_size > 0),
    state              TEXT        NOT NULL DEFAULT 'queued'
        CHECK (state IN ('queued', 'processing', 'succeeded', 'failed', 'cancelled')),
    attempts           INTEGER     NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    max_attempts       INTEGER     NOT NULL DEFAULT 4 CHECK (max_attempts > 0),
    -- Fencing token of the attempt holding the job; a stale attempt can't commit.
    lease_owner        UUID,
    lease_expires_at   TIMESTAMPTZ,
    cancel_requested   BOOLEAN     NOT NULL DEFAULT false,
    source_sha256      TEXT        CHECK (source_sha256 ~ '^[0-9a-f]{64}$'),
    result             JSONB,
    error_code         TEXT        CHECK (char_length(error_code) <= 64),
    error_detail       TEXT        CHECK (char_length(error_detail) <= 500),
    -- Last transient failure, for operators (never shown to users).
    last_error         TEXT        CHECK (char_length(last_error) <= 1000),
    -- NULL = needs a task (new, paused, or recovered); the reaper creates one.
    enqueued_at        TIMESTAMPTZ,
    -- Part of the task name (job-<id>-g<n>), so a re-enqueue is never deduplicated away.
    enqueue_generation INTEGER     NOT NULL DEFAULT 0,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    finished_at        TIMESTAMPTZ,
    UNIQUE (producer_id, idempotency_key),
    CHECK ((state IN ('succeeded', 'failed', 'cancelled')) = (finished_at IS NOT NULL)),
    CHECK (state <> 'processing' OR (lease_owner IS NOT NULL AND lease_expires_at IS NOT NULL)),
    CHECK (state <> 'failed' OR error_code IS NOT NULL)
);

CREATE INDEX IF NOT EXISTS jobs_active ON ingest.jobs (state, lease_expires_at)
    WHERE state IN ('queued', 'processing');
CREATE INDEX IF NOT EXISTS jobs_actor ON ingest.jobs (producer_id, actor)
    WHERE state IN ('queued', 'processing');
CREATE INDEX IF NOT EXISTS jobs_tenant ON ingest.jobs (producer_id, tenant, created_at);
CREATE INDEX IF NOT EXISTS jobs_finished ON ingest.jobs (finished_at) WHERE finished_at IS NOT NULL;

-- One row per terminal transition, in the same transaction: audit log and event mode.
CREATE TABLE IF NOT EXISTS ingest.job_events (
    id         BIGSERIAL   PRIMARY KEY,
    job_id     UUID        NOT NULL REFERENCES ingest.jobs (id) ON DELETE CASCADE,
    type       TEXT        NOT NULL CHECK (type IN ('succeeded', 'failed', 'cancelled')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS job_events_job ON ingest.job_events (job_id);

-- Kill switch. Scopes: 'global', 'producer:<id>', 'pipeline:<id>'.
CREATE TABLE IF NOT EXISTS ingest.switches (
    scope      TEXT        PRIMARY KEY CHECK (scope ~ '^(global|producer:[a-z0-9-]+|pipeline:[a-z0-9.-]+)$'),
    paused     BOOLEAN     NOT NULL,
    reason     TEXT        NOT NULL CHECK (char_length(reason) BETWEEN 1 AND 500),
    updated_by TEXT        NOT NULL CHECK (char_length(updated_by) BETWEEN 1 AND 200),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
