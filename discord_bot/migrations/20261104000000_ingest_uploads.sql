-- Uploads processed by the ingest worker (ADR 0004): TatarBot's side of the boundary.
-- The generic core (schema `ingest`, migrated separately) never sees these tables. There are
-- deliberately no foreign keys into `ingest`, so this migration doesn't depend on it.

-- One row per /upload interaction; `idempotency_key` is also the ingest job's key.
CREATE TABLE library_upload_requests (
    idempotency_key   TEXT        PRIMARY KEY CHECK (idempotency_key ~ '^discord-interaction:[0-9]+$'),
    job_id            UUID        UNIQUE,                  -- NULL until submitted
    application_id    BIGINT      NOT NULL,
    guild_id          BIGINT      NOT NULL,
    channel_id        BIGINT      NOT NULL,
    user_id           BIGINT      NOT NULL,
    title             TEXT        NOT NULL CHECK (char_length(title) BETWEEN 1 AND 100),
    attachment_url    TEXT        NOT NULL,
    declared_size     BIGINT      NOT NULL CHECK (declared_size BETWEEN 1 AND 26214400),
    -- Lets the app edit its deferred reply for 15 minutes; cleared on use or expiry.
    interaction_token TEXT,
    token_expires_at  TIMESTAMPTZ NOT NULL,
    submit_attempts   INTEGER     NOT NULL DEFAULT 0,
    -- Set when re-submission gave up; a 'submission_failed' notification is written then.
    abandoned_at      TIMESTAMPTZ,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX library_upload_requests_unsubmitted ON library_upload_requests (created_at)
    WHERE job_id IS NULL AND abandoned_at IS NULL;

-- Transactional outbox: written in the same transaction as the job's outcome (by the
-- worker's consumer hooks) and delivered by the bot, at least once.
CREATE TABLE discord_notifications (
    id                 BIGSERIAL   PRIMARY KEY,
    request_key        TEXT        NOT NULL UNIQUE REFERENCES library_upload_requests (idempotency_key) ON DELETE CASCADE,
    outcome            JSONB       NOT NULL CHECK (jsonb_typeof(outcome) = 'object'),
    state              TEXT        NOT NULL DEFAULT 'pending'
        CHECK (state IN ('pending', 'sending', 'delivered', 'undeliverable')),
    attempts           INTEGER     NOT NULL DEFAULT 0,
    next_attempt_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    lease_expires_at   TIMESTAMPTZ,
    -- Discord dedupes channel messages by nonce (with enforce_nonce) for a few minutes.
    nonce              TEXT        NOT NULL CHECK (char_length(nonce) BETWEEN 1 AND 25),
    delivered_via      TEXT        CHECK (delivered_via IN ('interaction', 'channel')),
    discord_message_id BIGINT,
    last_error         TEXT        CHECK (char_length(last_error) <= 1000),
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    delivered_at       TIMESTAMPTZ
);
CREATE INDEX discord_notifications_due ON discord_notifications (next_attempt_at)
    WHERE state IN ('pending', 'sending');
