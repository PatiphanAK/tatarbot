-- RAG store: documents split into chunks with embeddings, in their own schema so they
-- can share a database (Supabase) with the bot's music library.
--
-- `namespace` scopes retrieval (e.g. one per Discord server, or "global"); the runtime
-- treats it as an opaque string, like conversation_id. Re-ingesting a source replaces
-- its row; `content_hash` tells whether the content changed.
--
-- Embeddings are 1024-dimensional (bge-m3: multilingual, handles Thai). Changing the
-- model or dimension needs a new migration and a re-embed; `embedding_model` records
-- which model produced each vector so a mixed table can be detected.
--
-- pgvector is a trusted extension on Supabase; locally compose.yaml runs the
-- pgvector/pgvector image.
CREATE EXTENSION IF NOT EXISTS vector;

CREATE SCHEMA IF NOT EXISTS rag;

CREATE TABLE rag.documents (
    id           BIGSERIAL   PRIMARY KEY,
    namespace    TEXT        NOT NULL CHECK (char_length(namespace) BETWEEN 1 AND 200),
    -- URL, file name or any caller-chosen identifier; unique within a namespace.
    source       TEXT        NOT NULL CHECK (char_length(source) BETWEEN 1 AND 2000),
    title        TEXT        CHECK (char_length(title) <= 500),
    content_hash TEXT        NOT NULL CHECK (content_hash ~ '^[0-9a-f]{64}$'),
    metadata     JSONB       NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(metadata) = 'object'),
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (namespace, source)
);

CREATE TABLE rag.chunks (
    id              BIGSERIAL    PRIMARY KEY,
    document_id     BIGINT       NOT NULL REFERENCES rag.documents (id) ON DELETE CASCADE,
    -- Position within the document, for neighbouring-chunk context and stable re-ingest.
    ordinal         INTEGER      NOT NULL CHECK (ordinal >= 0),
    content         TEXT         NOT NULL CHECK (content <> ''),
    embedding       vector(1024) NOT NULL,
    embedding_model TEXT         NOT NULL CHECK (embedding_model <> ''),
    UNIQUE (document_id, ordinal)
);

-- Nearest-neighbour search by cosine distance (`embedding <=> $1`). HNSW needs no
-- training data, so it can be created on an empty table.
CREATE INDEX chunks_embedding_hnsw ON rag.chunks USING hnsw (embedding vector_cosine_ops);
