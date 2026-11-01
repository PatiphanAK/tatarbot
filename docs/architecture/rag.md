# Knowledge base (RAG) and pluggable embedding models

Status: implemented in `agent_runtime`, off by default (2026-10-29). Persistent store,
uploads and hybrid retrieval added 2026-11-01 (§7), tested against local Postgres and
RustFS only. Not yet enabled in ECS; no AWS resources exist for it.

TatarBot can answer from its own documents: Markdown and text files about the bot, the
community's rules, an FAQ. Before each chat turn the runtime finds the passages closest to
the user's message and puts them in the system message. Nothing else changes: the bot, the
HTTP contract of `/v1/chat`, the ReAct search agent and the guardrails work as before.

The embedding model is chosen by configuration, not code. Any server that speaks the
OpenAI Embeddings API works (vLLM, Ollama, Text Embeddings Inference, LiteLLM, OpenAI,
Alibaba Model Studio), and an offline hashing embedder covers development and tests.
Switching models re-indexes automatically.

## 1. Pipeline

```mermaid
flowchart LR
    subgraph index["Indexing: at startup and on POST /v1/knowledge/reload"]
        docs[("RAG_DOCS_DIR<br/>*.md, *.txt")] --> source[DirectoryDocumentSource]
        source --> chunk["chunk_document<br/>headings → paragraphs → windows"]
        chunk --> cache["CachedEmbeddings<br/>(RAG_CACHE_FILE, optional)"]
        cache -- "only new or changed chunks" --> embed[EmbeddingPort]
        cache --> store[(InMemoryVectorStore)]
    end

    subgraph turn["Every chat turn"]
        msg([user message]) --> q["EmbeddingPort.embed_query"]
        q --> search["VectorStore.search<br/>top_k, cosine"]
        search --> filter["min_score,<br/>max_context_chars"]
        filter --> sys["system message:<br/>excerpts marked as data"]
        sys --> agent[SearchAgent / model]
    end

    store -.-> search
```

| Step | Code | What happens |
|---|---|---|
| Load | `adapters/outbound/file_documents.py` | Every `.md`, `.markdown`, `.txt` under `RAG_DOCS_DIR`, recursively. Hidden paths, empty files and files over 1 MB are skipped. Id = relative path; title = first Markdown heading, else the file name. |
| Chunk | `domain/knowledge.py` | Split by Markdown headings (not inside code blocks), then paragraphs packed up to `RAG_CHUNK_CHARS`. Only a paragraph longer than that is cut, into windows overlapping by `RAG_CHUNK_OVERLAP`, at a space when there is one near the end (Thai often has none, so the cut is hard). Each chunk keeps its heading path, e.g. `Music > Uploading your own music`. |
| Embed | `EmbeddingPort.embed_documents` | The text embedded is `title > heading path`, a newline, then the chunk, so short chunks keep their context. |
| Store | `adapters/outbound/memory_vectors.py` | Vectors are normalised once; the whole index is swapped in one assignment, so a turn never sees a half-built index. |
| Retrieve | `application/knowledge.py` (`KnowledgeBase.retrieve`) | Embed the message (`embed_query`, with the query prefix), exact cosine search, keep hits with score ≥ `RAG_MIN_SCORE` until `RAG_MAX_CONTEXT_CHARS` is used. |
| Use | `application/chat_service.py` | The excerpts go into the system message, after the persona prompt, inside `<excerpt n=… title=…>` tags, with the rule that they are data, not instructions. They are not stored in history; the next turn retrieves again. |

### Design choices

* **Retrieve on every turn, not as a tool.** One embedding call (tens of milliseconds) and
  no extra model call. A `search_knowledge` tool in the ReAct loop would cost a model round
  trip and needs a model that calls tools reliably; the score threshold already leaves
  unrelated turns ("hello") without excerpts. If the knowledge base grows to the point where
  most turns retrieve something irrelevant, a tool is the next step.
* **Only the current message is the query.** A follow-up like "and for radio?" may retrieve
  nothing useful; the history still holds the previous answer. Query rewriting from history
  would cost a model call per turn.
* **Retrieval never fails a turn.** Embedding errors, timeouts (`RAG_TIMEOUT_SECONDS`) and
  dimension mismatches are logged and the turn goes on without knowledge, the same policy as
  a failed web search.
* **Indexing does not block startup.** The index builds in the background after the API is
  up. A cold or down embedding backend (scale-to-zero) leaves the runtime serving without
  knowledge; `POST /v1/knowledge/reload` retries. A failed reload keeps the previous index.
* **Exact search in memory, pure Python.** A hand-written knowledge base is tens to a few
  hundred chunks (the starter document is 7). Measured at 1,024 dimensions: about 25 ms for
  500 chunks, 100 ms for 2,000, run in a worker thread so other requests are not blocked.
  No database and no numpy dependency. `VectorStore` is a port, so numpy or pgvector (the
  bot already uses Postgres) can replace it without touching the pipeline.

## 2. Pluggable embedding models

Everything model-specific sits behind one port (`application/ports.py`):

```python
class EmbeddingPort(ABC):
    identity: str                                        # names the vector space
    async def embed_documents(self, texts) -> list[list[float]]
    async def embed_query(self, text) -> list[float]
```

`EMBEDDING_PROVIDER` selects the adapter in the composition root
(`main.py`, `build_embedder`):

| Provider | Adapter | Use |
|---|---|---|
| `openai` (default) | `adapters/outbound/openai_embeddings.py` | Any OpenAI-compatible `POST /embeddings`. Batches by `EMBEDDING_BATCH_SIZE`, sends `dimensions` only when `EMBEDDING_DIMENSIONS` is set, applies the query/document prefixes. |
| `hash` | `adapters/outbound/hash_embeddings.py` | No model, no network: hashed character trigrams. Matches shared wording only. For development, tests and CI. |

**Why queries and documents are separate calls.** Many retrieval models are trained with
different input for the two sides. The prefixes are configuration, so a new model needs no
code:

| Model family | `EMBEDDING_QUERY_PREFIX` | `EMBEDDING_DOCUMENT_PREFIX` |
|---|---|---|
| BGE-M3, OpenAI `text-embedding-3-*`, Alibaba `text-embedding-v3/v4` | (none) | (none) |
| multilingual-E5 | `query: ` | `passage: ` |
| Qwen3-Embedding | `Instruct: Find the passage that answers the question\nQuery: ` | (none) |

Check the model card of whatever you deploy; a missing prefix silently lowers quality.
`\n` in these variables is turned into a newline.

**Identity and re-indexing.** `identity` is derived from the provider, model name,
dimensions and a hash of the prefixes, e.g. `openai:bge-m3:native:1a2b3c4d` or
`hash:char3:512`. The embedding cache stores it, and a cache written under another identity
is discarded: change the model, restart (or reload), and everything is re-embedded once.
`GET /v1/knowledge` reports the identity of the live index. Vectors from different models
are never compared; a query whose dimension differs from the index is rejected.

**Adding a provider** (e.g. a native SDK, or an in-process sentence-transformers model):
one class implementing `EmbeddingPort`, one `case` in `build_embedder`, one value in
`EMBEDDING_PROVIDERS` (`config.py`). The pipeline, cache and store do not change.

### Choosing a model

TatarBot users write Thai and English, and documents may be in either, so the model must be
**multilingual** and map a Thai question near an English passage. Measured on the starter
document with the `hash` embedder: "how big can an uploaded file be?" finds the right
section (score 0.37), but "อัปโหลดเพลงได้ใหญ่สุดเท่าไหร่" scores 0.10 against everything, and
an unrelated "what is the capital of France?" scores 0.31, as high as a real match. That is
why `hash` is for development only.

Reasonable choices (verify availability and price before deploying):

| Where | Model | Notes |
|---|---|---|
| Same GPU as the chat model (vLLM) | `BAAI/bge-m3` or `Qwen/Qwen3-Embedding-0.6B` | A second vLLM process with `--task embed`, or a second Runpod endpoint; small (≈ 0.6 B parameters) |
| Local CPU (Ollama) | `bge-m3` | `ollama pull bge-m3`; `EMBEDDING_BASE_URL=http://localhost:11434/v1` |
| Alibaba Model Studio (already used for chat) | `text-embedding-v4` | Same compatible-mode base URL and key as the chat model, so only `EMBEDDING_MODEL` is needed |
| OpenAI | `text-embedding-3-small` | Set `EMBEDDING_BASE_URL` and `EMBEDDING_API_KEY` |

The embedding backend defaults to `MODEL_BASE_URL` and `MODEL_API_KEY`. When
`EMBEDDING_BASE_URL` points elsewhere, `MODEL_API_KEY` is **not** sent there; set
`EMBEDDING_API_KEY` if that host needs one.

## 3. Tuning

| Variable | Default | Effect |
|---|---|---|
| `RAG_TOP_K` | `4` | Candidates per turn |
| `RAG_MIN_SCORE` | `0.3` | Cosine similarity a candidate needs. **Model-specific**: calibrate it (below) |
| `RAG_MAX_CONTEXT_CHARS` | `4000` | Excerpt text per turn, about 1,000–2,000 tokens; candidates that would exceed it are skipped |
| `RAG_CHUNK_CHARS` | `1200` | Chunk size. Smaller = more precise hits, less context each |
| `RAG_CHUNK_OVERLAP` | `200` | Only for paragraphs longer than a chunk; must be < half of `RAG_CHUNK_CHARS` |
| `RAG_TIMEOUT_SECONDS` | `10` | Query embedding + search per turn |

**Calibrating `RAG_MIN_SCORE`.** Each turn logs
`knowledge retrieved hits=4 kept=2 top_score=0.612`. Ask a few questions the documents
answer and a few they don't ("hello", "capital of France"), in both languages, and set the
threshold between the two groups' top scores. Neural models typically separate them
somewhere in 0.3–0.6, but the scale differs per model.

## 4. Operating it

**Enable locally**

```sh
cd agent_runtime
# .env
RAG_DOCS_DIR=./knowledge
RAG_CACHE_FILE=./.cache/embeddings.json
EMBEDDING_PROVIDER=hash          # or: EMBEDDING_BASE_URL=http://localhost:11434/v1, EMBEDDING_MODEL=bge-m3
RAG_MIN_SCORE=0.3

make run
curl -s -H "Authorization: Bearer $AGENT_API_KEY" localhost:8080/v1/knowledge
```

**Container.** The image ships the starter documents at `/app/knowledge`
(`agent_runtime/knowledge/`). Set `RAG_DOCS_DIR=/app/knowledge`, or mount your own
directory and point at it. The root filesystem may be read-only: without `RAG_CACHE_FILE`
(or with an unwritable one, which is logged), every start re-embeds all chunks, which for a
small knowledge base is seconds.

**Edit the documents.** Change the files, then:

```sh
curl -s -X POST -H "Authorization: Bearer $AGENT_API_KEY" localhost:8080/v1/knowledge/reload
```

Only changed chunks are re-embedded (with a cache file). With a document directory baked
into the image, a deploy restarts the task, which re-indexes on its own.

**API**

| Method | Path | Auth | Response |
|---|---|---|---|
| `GET` | `/v1/knowledge` | Bearer | `{documents, chunks, embedder, indexed_at, duration_ms}`; `indexed_at` is `null` until the first index is built |
| `POST` | `/v1/knowledge/reload` | Bearer | Same body after re-indexing; 503 `embedding unavailable` if the embedding backend fails (the previous index stays) |

Both return 404 `knowledge base disabled` without `RAG_DOCS_DIR`. `/ready` is unchanged: the
knowledge base is optional and never makes the runtime unready.

**Logs**

| Line | Meaning |
|---|---|
| `knowledge indexed documents=… chunks=… embedder=… duration_ms=…` | An index was built |
| `embedding cache hits=… embedded=…` | How much of a re-index came from the cache |
| `embedding cache is for another embedder; re-embedding everything` | The model or its settings changed |
| `knowledge retrieved hits=… kept=… top_score=…` | Per turn; use it to tune `RAG_MIN_SCORE` |
| `knowledge retrieval failed` / `timed out` | The turn went on without knowledge |
| `chat turn completed … knowledge=N …` | Excerpts used in that turn |

## 5. Security

* **Documents are treated as data.** The prompt says excerpts are data, not instructions,
  the same rule as for web search results. Directory documents are written by operators.
  Uploads (§7) come from whoever holds the API key; if the bot ever lets ordinary members
  upload, a stored prompt injection reaches every turn in that server that retrieves it,
  so keep uploads to server admins.
* **Keys stay with their host** (§2). Embedding request headers are never logged; error
  bodies are logged truncated to 300 characters, and the caller only ever sees
  `embedding unavailable`.
* **Reload is behind the same bearer key as chat** and only re-reads the configured
  directory; it takes no path or content from the request.
* The output guardrails apply unchanged. `leaks_prompt` compares the reply with the persona
  prompt only, so quoting a knowledge document is allowed.

## 6. Limits and next steps

* No ECS wiring yet: add the `RAG_*` / `EMBEDDING_*` variables to the runtime task
  definition in `infra/aws/ecs.tf` (and the API key to SSM if the embedding host differs),
  and the bucket, queue, worker service and IAM of §7.6.
* The integration mock (`integration/mock_openai.py`) has no `/embeddings` route; the
  OpenAI-compatible adapter is covered by unit tests with a mocked HTTP transport only.
* Single-message queries (§1). Possible improvements, in order of value: query rewriting
  from history, a reranker model. (Hybrid scoring and pgvector: done, §7.)

## 7. Persistent store, uploads and asynchronous ingestion

Off unless `RAG_STORE=postgres`. Uploads additionally need `RAG_S3_BUCKET` and
`RAG_SQS_QUEUE_URL`. With `RAG_STORE=memory` (the default) everything above is unchanged.

### 7.1 Flow

```mermaid
sequenceDiagram
    participant C as Client (bot / operator)
    participant R as Runtime API
    participant DB as Postgres (rag schema)
    participant S3 as S3 (private bucket)
    participant Q as SQS
    participant W as Worker

    C->>R: POST /v1/knowledge/uploads {namespace, filename, size_bytes}
    R->>R: bearer key, namespace, extension, size
    R->>DB: document (pending_upload, server-generated object key)
    R-->>C: presigned POST (key fixed, 1..RAG_UPLOAD_MAX_BYTES)
    C->>S3: POST file (S3 rejects other keys and sizes)
    S3->>Q: ObjectCreated event
    W->>Q: receive (long poll)
    W->>DB: find document by (bucket, key), ensure + claim job
    W->>S3: HEAD (size), GET (If-Match / VersionId)
    W->>W: detect type, parse, chunk, embed
    W->>DB: one transaction: replace chunks, document ready, job succeeded
    W->>Q: delete message
    C->>R: GET /v1/knowledge/documents/{id}?namespace= (poll status)
```

| Piece | Code |
|---|---|
| Schema | `db/migrations/20261101000000_rag_ingestion.sql` (additive; the first migration is untouched) |
| Use cases | `application/ingestion.py`: `Uploads` (create, list, delete, reprocess), `Ingestor` |
| Ports | `application/ports.py`: `DocumentRepository`, `ObjectStorage`, `IngestQueue`; `VectorStore.search` gained `text` and `namespaces` |
| Postgres | `adapters/outbound/postgres_rag.py`: `PostgresVectorStore`, `PostgresDocumentRepository` |
| AWS | `adapters/outbound/s3_storage.py`, `adapters/outbound/sqs_queue.py` (boto3) |
| Worker | `adapters/inbound/sqs_worker.py`, composed in `worker.py` (`python -m agent_runtime.worker`) |
| Validation | `domain/ingestion.py`: accepted types, parsing, namespaces |

### 7.2 What is trusted

Nothing the client says about the file:

* **Key**: `RAG_S3_PREFIX` + random hex + the extension, generated by the runtime. The
  presigned policy pins it; a client cannot write anywhere else.
* **Size**: the presigned POST's `content-length-range` is enforced by S3; the worker checks
  `ContentLength` before downloading and reads at most the limit plus one byte.
* **Type**: decided from the bytes: common binary signatures (PDF, ZIP/Office, images) are
  rejected, the content must be strict UTF-8 without control characters. The extension
  (`.md`, `.markdown`, `.txt`) only chooses Markdown or plain text. The stored `mime_type`
  is the detected one; the client's Content-Type is never read.
* **Name**: only the base name is kept (`../../etc/passwd.md` becomes `passwd.md`), as the
  default `source` and fallback title. It never reaches a path or a key.

Only text and Markdown are accepted. PDF or DOCX would need a parser dependency and a new
`PARSER_VERSION`.

### 7.3 Idempotency, retries and partial failures

* **Job identity.** `rag.ingestion_jobs` is unique on (document, object key, object version,
  parser version, chunker version, embedding model). S3 delivers events at least once; a
  duplicate finds the finished job and is acknowledged without work. The object version is
  the S3 version id, or `etag:<ETag>` on an unversioned bucket (read with `If-Match`, so an
  overwrite is detected).
* **Claiming.** A worker claims a job by setting it to `processing` and incrementing
  `attempts`, under row locks (document, then job, in every code path, so workers and
  deletes cannot deadlock). A job processing for longer than `RAG_INGEST_LEASE_SECONDS` is
  taken over (a crashed worker); a live one makes the message retry later.
* **Atomic swap.** Chunks are deleted and inserted in the same transaction that marks the
  document ready, and only if the attempt still holds its claim and the document still
  points at that object. Retrieval therefore sees the previous complete version or the
  new one, never a mix or a half-written document.
* **Failures.** Content that can never work (type, size, empty, missing object) fails the
  job at once and the message is acknowledged. Anything else (embedding backend, S3,
  database, bugs) puts the job back to `queued` with the error; the worker extends the
  message's visibility with exponential backoff (30 s, 60 s, ... up to 15 min) and SQS
  redelivers it. After `RAG_INGEST_MAX_ATTEMPTS` the job and document are `failed`.
  A failed run never removes the document's live chunks: a broken re-upload keeps serving
  the last good version, and `status`/`error` say what happened.
* **Re-uploads.** Requesting an upload for an existing `(namespace, source)` points the
  document at a new key and deletes the previous object. A late event for the old key
  finds no document and is ignored. Identical content (same SHA-256 under the same
  pipeline configuration) is not re-embedded.
* **Reprocessing.** `POST /v1/knowledge/documents/{id}/reprocess` queues the document. With
  a changed embedding model, chunker or parser version it is a new job and the document is
  re-embedded. Otherwise it does nothing. `force=true` re-runs a finished or failed job.
  Retrieval only searches chunks of the configured embedding model, so during a model
  change, documents not yet re-embedded are invisible rather than compared across models.
* **Deletion.** Deleting a document removes its chunks and jobs (`ON DELETE CASCADE`) in one
  statement, so retrieval stops at once; the object is deleted afterwards, best effort
  (an orphan costs storage only).

### 7.4 Hybrid retrieval

One SQL statement (`_HYBRID_SEARCH` in `postgres_rag.py`) builds two rankings over chunks
in the turn's namespaces and of the configured embedding model:

1. **Semantic**: `ORDER BY embedding <=> query` over the HNSW index, `5 × top_k` candidates
   (at least 20), with `hnsw.iterative_scan = relaxed_order` so the filters cannot starve
   the index scan.
2. **Keyword**: `query <% content` (pg_trgm word similarity above
   `RAG_KEYWORD_MIN_SCORE`, GIN trigram index), ordered by similarity.

They are fused with Reciprocal Rank Fusion, `score = Σ 1 / (60 + rank)`. The returned
`score` is still the cosine similarity, so `RAG_MIN_SCORE` keeps its meaning; a chunk the
keyword ranking found is kept even below it. That is the point: an exact command or name
the embedding model misses still reaches the model.

**Thai.** Postgres full-text search (`to_tsvector('simple', …)`) keeps a Thai phrase as one
token, because Thai has no spaces between words, so it cannot match a word inside a
sentence. Trigrams work on characters. Measured on the test database:

| Query | Relevant chunk | Unrelated chunk |
|---|---|---|
| `อัปโหลดเพลงได้ใหญ่สุดเท่าไหร่` | 0.28 | 0.00 |
| `how big can an uploaded file be` | 0.34 | 0.03 |
| `/upload` | 1.00 | 0.14 |
| `capital of France` | 0.06 | 0.00 |

Hence the 0.25 default. Limits: trigram similarity drops for long queries (only the first
500 characters are used), and a Thai query and a document that spell a word differently,
or use synonyms, do not match. The semantic ranking (a multilingual model) covers those.
pg_trgm needs a database whose `LC_CTYPE` classifies Thai letters as alphanumeric
(`en_US.UTF-8` does; a `C` locale does not). `test_trigram_matching_handles_thai` checks
this. Run it, or the query in it, against the production database before relying on it.

### 7.5 Namespaces and access

* A chat turn reads `global` plus `discord:<guild id>`, derived on the server from the bot's
  conversation id (`discord:<guild>:<channel>:<user>`); DMs read only `global`. Nothing in
  the chat request selects a namespace, so one server's documents never reach another's.
* Upload and document endpoints take the namespace as a parameter and only ever see rows in
  it (a document id from another namespace is a 404). They are behind the same bearer key
  as chat: the runtime trusts its key holders (the bot, operators) to have checked that the
  user may manage that namespace's documents (e.g. Discord's Manage Server permission).
  There is no per-user authorization in the runtime.
* `RAG_DOCS_DIR` documents are indexed into `global`; they never overwrite an upload with
  the same source, and uploads cannot take over a directory document's source.

### 7.6 Deployment requirements (not built yet)

* **Database**: `make -C agent_runtime migrate` against `DATABASE_URL`. Needs the `vector` and `pg_trgm`
  extensions (both available on Supabase).
* **Bucket**: private (public access block, bucket-owner-enforced ownership, SSE), with a
  CORS rule allowing `POST` from the uploading origin only if browsers upload directly.
  A lifecycle rule expiring objects under `RAG_S3_PREFIX` that no document references is
  optional (deleted documents' objects are already removed). If versioning is enabled,
  also expire noncurrent versions, since `DeleteObject` only adds a delete marker.
* **Notifications**: S3 → SQS directly (`s3:ObjectCreated:*`, prefix `RAG_S3_PREFIX`). The
  queue policy must allow `s3.amazonaws.com` with `aws:SourceArn` = the bucket. Visibility
  timeout ≥ the longest ingestion (embedding a 1 MB document); a dead-letter queue with
  `maxReceiveCount` above `RAG_INGEST_MAX_ATTEMPTS`.
* **Worker**: a separate ECS service (same image, command
  `python -m agent_runtime.worker`) with the runtime's `RAG_*`, `EMBEDDING_*`,
  `DATABASE_URL` environment. It must use the same embedding and chunking settings as the
  runtime, or its chunks are invisible to retrieval.
* **IAM (least privilege)**: no keys in configuration; task roles only.

  | Role | Actions | Resource |
  |---|---|---|
  | Runtime task | `s3:PutObject` (signing presigned POSTs), `s3:DeleteObject` | `arn:aws:s3:::<bucket>/<prefix>*` |
  | Runtime task | `sqs:SendMessage` | the queue |
  | Worker task | `s3:GetObject` (+ `s3:GetObjectVersion` if versioned) | `arn:aws:s3:::<bucket>/<prefix>*` |
  | Worker task | `sqs:ReceiveMessage`, `sqs:DeleteMessage`, `sqs:ChangeMessageVisibility`, `sqs:GetQueueAttributes` | the queue |
  | Both | KMS `Decrypt`/`GenerateDataKey` | only if the bucket or queue uses a customer-managed key |

  A presigned POST carries the signer's permissions, so the runtime role must not be able
  to write outside the prefix.
