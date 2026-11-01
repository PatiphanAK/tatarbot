# domain

Types and policies that don't depend on any transport, provider or storage. Nothing here
imports FastAPI, httpx, psycopg, boto3 or LangGraph, or knows about Discord, vLLM or S3;
everything is a pure function or a frozen dataclass and is unit-tested without fakes.

| Module | Responsibility |
|---|---|
| `inference.py` | The provider-agnostic contract with a model: `ChatMessage`, `InferenceRequest`, `InferenceResult`, `ToolSpec`, `ToolCall` |
| `errors.py` | Error taxonomy the adapters raise and the HTTP adapter maps to status codes (`ModelError` subclasses → 502/503/504, `EmbeddingError`, `StorageError` → 503, `InvalidInputError` → 422) |
| `language.py` | Thai/English detection by script and the reply-language directive |
| `guardrails.py` | Input refusal (prompt-injection patterns), output checks (prompt leaks, secret-looking strings) |
| `client_tools.py` | Tools the caller carries out (`play_music`): the tool schema offered to the model, argument validation, and how actions and their outcomes are stored in history |
| `search.py` | `SearchResult`, as the agent sees web results |
| `knowledge.py` | `Document`, `Chunk`, `RetrievedChunk`; Markdown-aware chunking (`chunk_document`) and `CHUNKER_VERSION` |
| `ingestion.py` | Uploaded documents: accepted types, `parse_document`, namespace rules (`retrieval_namespaces`), job identity (`JobKey`), `PARSER_VERSION`, `DocumentStatus` |

## Invariants

* `CHUNKER_VERSION` and `PARSER_VERSION` are part of every ingestion job's identity. Bump
  one whenever its output changes for the same input, so stored documents can be
  re-ingested with `reprocess`; leave them alone for refactors.
* `retrieval_namespaces` is the only place that decides which knowledge a chat turn may
  read. It derives namespaces from the conversation id, never from request fields.
* Error messages of `InvalidInputError` subclasses are returned to callers, so they must
  not contain internals.

## Extending

New policy: add a module with pure functions and its own `tests/test_<module>.py`, and
call it from a use case in `application/`. Anything that needs I/O belongs behind a port.
