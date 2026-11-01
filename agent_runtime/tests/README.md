# agent_runtime tests

From `agent_runtime/`:

```sh
make test      # unit tests; tests that need Postgres are skipped
make test-db   # starts the local pgvector Postgres and runs everything
make check     # ruff lint + format check, then tests
uv run pytest tests/test_ingestion_units.py -k worker -q   # a subset
```

## Layout

| Files | Cover | Database |
|---|---|---|
| `test_api.py`, `test_knowledge_api.py` | HTTP contract: auth, validation, error mapping, knowledge endpoints | no |
| `test_chat_service.py`, `test_agent.py`, `test_client_tools.py` | chat use case, ReAct loop, `play_music` tool and action history | no |
| `test_guardrails.py`, `test_language.py` | domain policies | no |
| `test_vllm.py`, `test_vllm_tools.py`, `test_tavily.py`, `test_embeddings.py` | outbound HTTP adapters with `httpx.MockTransport` | no |
| `test_knowledge.py`, `test_memory_store.py` | in-memory RAG pipeline and history store | no |
| `test_ingestion_units.py` | parsing and validation, SQS worker message handling, S3 adapter (botocore `Stubber`), upload routes, config validation, hybrid-hit filtering | no |
| `test_migrate.py` | migration runner; schema constraints; compatibility with rows from earlier migrations | partly |
| `test_postgres_rag.py` | `PostgresVectorStore`: persistence, namespaces, embedding-model filter, hybrid ranking, Thai keywords, dimension checks | yes |
| `test_ingestion.py` | upload lifecycle against real Postgres with fake S3/SQS: duplicate events, retries, give-up, lease takeover, re-uploads, delete cascade, reprocess after a model change | yes |

## Fixtures (`conftest.py`)

* `settings`: a minimal valid `Settings`. `FakeModel` scripts model replies and records
  requests.
* `db` marker: skips a test unless `TEST_DATABASE_URL` is set (`make test-db` sets it to
  the compose Postgres).
* `database`: creates a fresh database for one test and drops it afterwards, so database
  tests are isolated and can run against a development server. The server must have the
  `vector` extension available (the `pgvector/pgvector` image).

Async tests use `pytest.mark.anyio` on the asyncio backend.

## Conventions

* Prefer real collaborators where they are cheap (Postgres in a throwaway database, the
  `hash` embedder at 1024 dimensions) and fakes at process boundaries (model, S3, SQS).
* A test for a race or a failure path should fail if the guard it covers is removed.
* No network access: live services are never called from this suite.
