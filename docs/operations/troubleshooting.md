# Troubleshooting

Symptoms, the log line that confirms each one, and the fix. Log lines are quoted from the
code, so they can be searched. Logs: `docker compose logs <service>` locally, CloudWatch
log groups in ECS (14 days). Infrastructure operations (deploy, rollback, stop) are in
[infra/aws/README.md](../../infra/aws/README.md#day-to-day-operations).

## Discord bot

| Symptom | Log | Cause → fix |
|---|---|---|
| Bot exits at startup | `another TatarBot process with this token is already running (pid …)` | A second process with the same token on this machine. Stop it; two bots answer every command. |
| Bot exits at startup | `Discord rejected the MESSAGE_CONTENT intent: …` | Enable *Message Content Intent* in the Developer Portal (Bot → Privileged Gateway Intents). |
| Commands sometimes fail with "Unknown interaction", `/stop` doesn't stop | defers succeed with small `defer_ms` | Another process (another machine or a stray container) answers first. Run exactly one bot per token. |
| Chat answers "⚠️ I can't reach my brain right now…" | `agent runtime rejected credentials; check AGENT_API_KEY` | The bot's and the runtime's `AGENT_API_KEY` differ. Without that line: the runtime is down or the model failed (see below). |
| Chat answers "⌛ That took too long…" | runtime logs `model error: …` timeout | Model slower than `MODEL_TIMEOUT_SECONDS`, often a scale-to-zero cold start. Retry; measure cold starts before raising timeouts (they must stay below `AGENT_TIMEOUT_SECS`). |
| Library commands say "temporarily unavailable" | `library database unavailable` (with `retry_in`) | Postgres unreachable. The bot keeps retrying (backoff up to 5 min) and keeps playing radio and links. |
| Same, for a long time | `library database unreachable for over 10 minutes; if it is a Supabase free project it may be paused …` | Supabase paused the free project. Use *Resume project* in the dashboard. |
| Thai song titles only match exact substrings | `title search: fuzzy for Latin; Thai by substring only (database LC_CTYPE …)` | The database's `LC_CTYPE` is `C`. Expected and handled; fuzzy Thai needs an `en_US.UTF-8` database. |
| Radio cuts out and comes back, or stops | `radio reconnect failed` | The station is down or unreachable. The bot gives up after 5 failures in a row; a minute of good playback resets the count. |
| No autocomplete suggestions | `autocomplete search too slow` | Search exceeded its 2 s budget (slow or waking database). No action needed if rare. |

## Agent runtime

| Symptom | Log | Cause → fix |
|---|---|---|
| Runtime won't start | `missing required environment variable …` / `invalid value for …` | Fix `.env` / the task definition. Messages name the variable. |
| Runtime won't start with `RAG_STORE=postgres` | `the RAG schema stores 1024-dimensional vectors; EMBEDDING_DIMENSIONS is …` | The configured embedding size isn't 1024. Use bge-m3 (or set `EMBEDDING_DIMENSIONS=1024` on a model that supports it). `hash` defaults to 512: set 1024. |
| `/ready` returns 503 | – | The model backend is down or doesn't serve `MODEL_NAME`. Check `MODEL_BASE_URL`, `MODEL_NAME`, `MODEL_API_KEY`. Note that `/ready` wakes a scale-to-zero endpoint. |
| Answers ignore the knowledge base | `knowledge retrieved hits=… kept=0 … top_score=…` | Nothing scored above `RAG_MIN_SCORE`. Calibrate it per model ([rag.md §3](../architecture/rag.md#3-tuning)). |
| Same | `knowledge retrieval failed: …` / `knowledge retrieval timed out` | Embedding backend or database down/slow. The turn continues without knowledge. |
| Same, right after start | `knowledge indexing failed, retry with POST /v1/knowledge/reload: …` | Embedding backend was down during the initial index. Call the reload endpoint once it's up. |
| Same, with Postgres | `rag.chunks.embedding is missing: run the migrations` | Run `make -C agent_runtime migrate` against `DATABASE_URL`. |
| Same, after changing the embedding model | `GET /v1/knowledge` shows `chunks: 0` | Retrieval only reads chunks of the configured model. Restart/reload for `RAG_DOCS_DIR`, and reprocess uploaded documents (`POST …/documents/{id}/reprocess`). |
| A directory file is missing from the index | `knowledge file … skipped: an upload has that source` | An uploaded document in `global` has the same source. Rename one of them. |
| Replies refused | `input refused conversation_id=… reason=…` | Input guardrail matched. Inspect `reason`; patterns are in `domain/guardrails.py`. |
| Web search silently absent | `web search failed: …` / `search returned HTTP …` | Tavily key or quota. The agent answers without search. |
| `make -C agent_runtime migrate` fails | `migration … was modified after it was applied` | An applied migration file was edited. Restore it and add a new migration instead. |

## Upload ingestion (worker)

The document's `status` and `error` (`GET /v1/knowledge/documents/{id}?namespace=`) are the
first place to look.

| Status / symptom | Log | Cause → fix |
|---|---|---|
| Stays `pending_upload` | no `ingestion finished` | The file never reached S3 (URL expired after `RAG_UPLOAD_URL_TTL_SECONDS`, or the POST was rejected), or no event arrived: check the bucket's notification configuration and the queue policy. |
| `failed`, error names the file type, size or encoding | `ingestion rejected document_id=…` | Permanent: only UTF-8 `.md`/`.txt` within `RAG_UPLOAD_MAX_BYTES`. Upload a valid file. |
| `failed`, `the uploaded file is missing` | `ingestion: object gone …` | Object deleted or overwritten out of band. Upload again. |
| `queued` with an `EmbeddingError: …` or `StorageError: …` error | `ingestion failed document_id=… retry=True` | Transient. SQS redelivers with backoff (30 s doubling to 15 min). Fix the embedding backend or S3 access. |
| `failed`, `gave up after N attempts` | – | `RAG_INGEST_MAX_ATTEMPTS` reached. After fixing the cause: `POST …/reprocess?namespace=…&force=true`. |
| `processing` for a long time | – | A worker crashed mid-job. The job is taken over after `RAG_INGEST_LEASE_SECONDS` on the next delivery. |
| Ingested but never retrieved | – | The worker's `EMBEDDING_*` or `RAG_CHUNK_*` differ from the runtime's (chunks are filtered by embedding model), or the document's namespace isn't `global` / the server's. |
| Messages pile up in the dead-letter queue | `dropping malformed …` / `ingestion message failed` | Inspect a message. Malformed ones are dropped, not dead-lettered; DLQ messages failed `maxReceiveCount` times. |
| Worker exits at startup | `the worker needs RAG_STORE=postgres and RAG_S3_BUCKET` / `missing required environment variable …` | The worker reads the runtime's full configuration, including `AGENT_API_KEY`, `MODEL_BASE_URL` and `MODEL_NAME`. |

## Database

| Symptom | Cause → fix |
|---|---|
| `CREATE EXTENSION vector` fails locally | The `postgres` container predates the pgvector image; recreate it ([development.md](../development.md#local-backing-services)). |
| Thai keyword search finds nothing | `SELECT show_trgm('เพลง')` returns `{}`: the database's `LC_CTYPE` is `C`. Semantic search still works. |
| Too many connections on Supabase | The bot (`DATABASE_MAX_CONNECTIONS`, 3) and the runtime and worker (`DATABASE_POOL_SIZE`, 5 each) share the project's limit. Lower the pools. |
