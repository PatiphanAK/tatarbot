# ADR 0002: Persistent knowledge store and upload ingestion

- Status: Accepted (implemented on `feat/rag-db-migrations`, not deployed)
- Date: 2026-11-01
- Details: [Knowledge base (RAG) §7](../architecture/rag.md#7-persistent-store-uploads-and-asynchronous-ingestion),
  [Database](../architecture/database.md)

## Context

The knowledge base started in memory: documents from `RAG_DOCS_DIR`, re-embedded at every
start, exact cosine search in Python, one global corpus. That works for a hand-written set
of a few hundred chunks. It doesn't handle documents added at run time, per-server
documents, or exact-term queries (command names, proper nouns) that embedding models often
rank poorly. Users write Thai and English.

Already in place: Postgres (Supabase free tier) for the bot's music library, S3 for its
audio, and a single-task runtime with in-memory conversation history.

## Decision

1. **pgvector in the existing Postgres**, in a separate `rag` schema with its own migration
   runner and history table (`agent_runtime/src/agent_runtime/db/`), next to the bot's sqlx
   schema. `vector(1024)` for BGE-M3 with an HNSW cosine index. The size is fixed in the
   schema and checked by the runtime; another model size needs a migration and a re-embed.
2. **Hybrid retrieval in one SQL statement:** HNSW nearest neighbours plus `pg_trgm` word
   similarity, fused with Reciprocal Rank Fusion (k = 60). Trigrams, not full-text search,
   because the `simple` text-search parser keeps a Thai phrase as one token.
3. **Uploads go straight to a private S3 bucket through presigned POST.** The runtime only
   issues the URL and records a pending document. S3 `ObjectCreated` events go to SQS, and a
   separate worker process (`python -m agent_runtime.worker`) ingests them.
4. **Ingestion is idempotent by job identity:** one `rag.ingestion_jobs` row per (document,
   object key and version, parser, chunker and embedding model versions). A document's
   chunks are replaced whole in one transaction.
5. **Namespaces**: `global` plus `discord:<guild id>`, derived from the bot's conversation
   id on the server side.

## Alternatives considered

- **A managed vector database (Pinecone, Qdrant Cloud, OpenSearch):** another service,
  account and bill, and no transactions spanning documents, jobs and chunks. Postgres is
  already there.
- **Full-text search (`tsvector`) for the keyword side:** no Thai word segmentation in
  Postgres. A Thai segmenter (an extension, or segmenting in Python before indexing) would
  be a new dependency in a hosted database where extensions are restricted.
- **Upload through the runtime API (multipart):** the file transits the runtime and its
  memory, and the runtime has to enforce size limits itself. Presigned POST lets S3 enforce
  the key and the size range.
- **Ingest inline in the API request, or in a background task of the API process:** a slow
  or cold embedding backend would hold requests open, and work would be lost on every
  deploy (the runtime runs one task). SQS gives redelivery, backoff and a dead-letter queue.
- **SNS fan-out or EventBridge between S3 and SQS:** nothing else consumes the events yet.

## Consequences

Positive:
- Documents survive restarts and deploys; no re-embedding at startup.
- Exact terms and Thai substrings are found even when the embedding misses them.
- One server's documents never reach another server's chats.
- Duplicate S3 events, worker crashes and transient embedding outages are handled without
  operator action.

Negative / to manage:
- New AWS resources (bucket, queue, DLQ, worker service, IAM) that don't exist yet; see
  [rag.md §7.6](../architecture/rag.md#76-deployment-requirements-not-built-yet).
- The worker is a second long-running process per environment.
- Retrieval now depends on the database: a Supabase pause or outage removes knowledge from
  chat turns (the turn still succeeds without it).
- Trigram matching of Thai depends on the database's `LC_CTYPE`. It's verified locally
  (`en_US.UTF-8`), not on Supabase.
- `boto3` and `psycopg-pool` join the runtime's dependencies (+24 MB image).

## Revisit when

- The corpus grows to where HNSW recall or the 1024-d storage cost matters: consider
  halfvec, quantization, or a dedicated store.
- Members other than admins should upload: per-user authorization and prompt-injection
  review of uploaded content.
- PDF or Office documents are needed: a parser dependency and a new `PARSER_VERSION`.
- A Thai segmenter becomes available in the hosted database: compare it with trigrams.
