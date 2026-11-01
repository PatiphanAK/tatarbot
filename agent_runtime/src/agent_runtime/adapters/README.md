# adapters

Concrete implementations of the application's ports (outbound) and the entry points that
call use cases (inbound). Only adapters import frameworks, HTTP clients, database drivers
and AWS SDKs. Only the composition roots (`../main.py`, `../worker.py`) construct them.

## Inbound

| Adapter | Drives | Notes |
|---|---|---|
| `inbound/http_api.py` | `ChatService`, `KnowledgeBase`, `Uploads` | FastAPI router, bearer auth, request validation (pydantic), error mapping to fixed messages. Contract: [docs/architecture/api.md](../../../../docs/architecture/api.md) |
| `inbound/sqs_worker.py` | `Ingestor` | Long-polls SQS; S3 event and reprocess messages; acknowledges `DONE`, backs off on `RETRY`; drops malformed messages |

## Outbound

| Port | Adapter | Selected by |
|---|---|---|
| `ModelPort` | `vllm.py`: OpenAI Chat Completions client (vLLM, Runpod, Alibaba, any compatible API) | always |
| `SearchPort` | `tavily.py` | `TAVILY_API_KEY` set |
| `ConversationStore` | `memory_store.py` (in process, bounded, lost on restart) | always |
| `EmbeddingPort` | `openai_embeddings.py` (`POST /embeddings`), `hash_embeddings.py` (offline), wrapped by `embedding_cache.py` (JSON file) | `EMBEDDING_PROVIDER`, `RAG_CACHE_FILE` |
| `DocumentSource` | `file_documents.py` (`.md`/`.txt` under a directory) | `RAG_DOCS_DIR` |
| `VectorStore` | `memory_vectors.py` (exact cosine), `postgres_rag.py` `PostgresVectorStore` (pgvector + trigram, RRF) | `RAG_STORE` |
| `DocumentRepository` | `postgres_rag.py` `PostgresDocumentRepository` | `RAG_STORE=postgres` + `RAG_S3_BUCKET` |
| `ObjectStorage` | `s3_storage.py` (boto3, presigned POST) | `RAG_S3_BUCKET` |
| `IngestQueue` | `sqs_queue.py` | `RAG_SQS_QUEUE_URL` |

## Rules every adapter follows

* **Translate failures into domain errors** (`ModelError` subclasses, `SearchError`,
  `EmbeddingError`, `StorageError`, `ObjectMissingError`, `InvalidDocumentError`) with
  messages that carry no credentials; upstream bodies are logged truncated, never returned.
* **Never block the event loop:** synchronous SDKs (boto3) run in `asyncio.to_thread`;
  CPU-heavy work (in-memory search) too.
* **Credentials** come from configuration passed in by the composition root, or for AWS
  from the default credential chain; adapters never read secrets from the environment
  themselves.
* `aclose()` releases connections; the lifespan in `main.py` calls it.

## Adding an implementation

Example: a new embedding provider.

1. A class implementing `EmbeddingPort` here. `identity` must change with anything that
   changes the vectors (model, dimensions, prefixes).
2. One `case` in `build_embedder` (`main.py`) and its name in `EMBEDDING_PROVIDERS`
   (`config.py`).
3. Tests with a mocked transport (see `tests/test_embeddings.py`).

The same pattern holds for other ports: implement the ABC, select it in `main.py` from a
setting, document the setting in the runtime README and `.env.example`.

## Testing

HTTP clients are tested with `httpx.MockTransport` (`test_vllm.py`, `test_embeddings.py`,
`test_tavily.py`), S3 with botocore's `Stubber` (`test_ingestion_units.py`), Postgres
adapters against a real database (`test_postgres_rag.py`, `test_ingestion.py`; `make test-db`).
