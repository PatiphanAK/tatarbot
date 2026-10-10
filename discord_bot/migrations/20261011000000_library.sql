-- Per-server audio library. Audio lives in S3 under <prefix><content_hash>.ogg; one object
-- may back rows in several servers (deduplicated by the hash of the uploaded original).
-- Discord ids are stored as BIGINT (snowflakes fit in 63 bits).
CREATE TABLE library_tracks (
    id           BIGSERIAL   PRIMARY KEY,
    guild_id     BIGINT      NOT NULL,
    content_hash TEXT        NOT NULL CHECK (content_hash ~ '^[0-9a-f]{64}$'),
    title        TEXT        NOT NULL CHECK (char_length(title) BETWEEN 1 AND 100),
    duration_ms  INTEGER     NOT NULL CHECK (duration_ms > 0),
    -- Size of the stored (transcoded) object; counted in full against each server's quota.
    size_bytes   BIGINT      NOT NULL CHECK (size_bytes > 0),
    uploaded_by  BIGINT      NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (guild_id, content_hash)
);

-- Reference counting on delete, and dedupe on upload.
CREATE INDEX library_tracks_content_hash ON library_tracks (content_hash);
-- Listing and title search within one server.
CREATE INDEX library_tracks_guild_title ON library_tracks (guild_id, lower(title));
