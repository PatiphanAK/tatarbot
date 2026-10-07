# TatarBot Agent Runtime

A small orchestration service between TatarBot and the inference backend. It owns
conversation history and the system prompt, and turns chat turns into generic inference
requests. It knows nothing about Discord, and its callers know nothing about the model.

```text
TatarBot ──POST /v1/chat──▶ Agent Runtime ──POST /v1/chat/completions──▶ vLLM (Qwen)
```

## Architecture

```text
adapters/inbound/http_api.py      FastAPI: auth, validation, error mapping
        │
        ▼
application/chat_service.py       ChatService: history + system prompt -> InferenceRequest
        │            │
        ▼            ▼
 ConversationStore  ModelPort      application/ports.py (abstract)
        │            │
        ▼            ▼
 memory_store.py    vllm.py        adapters/outbound/
```

* `domain/inference.py` defines the generic contract: `InferenceRequest` (messages + params),
  `InferenceResult`, and `ChatMessage`. It contains no model name, URL, HTTP or Discord types.
* `ChatService` depends only on the two ports. Possible replacements:
  * `ModelPort`: `VLLMModel` today, later `OllamaModel`, `OpenAIModel`, or a queue/scheduler
    adapter that hands `InferenceRequest`s to GPU workers.
  * `ConversationStore`: in-memory today, Redis/Postgres later.
  * HTTP inbound: could be joined by an event/queue consumer that calls the same `ChatService`.
* `main.py` (`create_app`) is the composition root.

## API

| Method | Path | Auth | Purpose |
|---|---|---|---|
| `POST` | `/v1/chat` | Bearer | `{message, conversation_id}` → `{message, conversation_id}` |
| `GET` | `/health` | – | Liveness: the process is up |
| `GET` | `/ready` | – | Readiness: the backend is reachable and serves `MODEL_NAME` (otherwise 503) |

Errors are always `{"error": "<fixed message>"}`. Upstream bodies, URLs, keys and tracebacks
are never returned to the caller.

| Situation | Status |
|---|---|
| Missing or invalid API key | 401 |
| Invalid body / blank message | 422 |
| Backend rejected the request (4xx) or returned a malformed/empty reply | 502 |
| Backend unreachable, 5xx, or 429 | 503 |
| Backend timed out | 504 |
| Unexpected error | 500 |

## Configuration

See `.env.example`. Required: `AGENT_API_KEY`, `MODEL_BASE_URL`, `MODEL_NAME`.

Qwen3 is a reasoning model. By default the runtime sends
`chat_template_kwargs.enable_thinking=false` and strips any `<think>…</think>` from replies.

History is kept in memory: the last `MAX_HISTORY_MESSAGES` messages are sent per turn, and
everything is lost on restart. Turns within one conversation are serialized per process.

## Development

```sh
make install   # uv sync
make env       # .env with a generated AGENT_API_KEY (put the same key in discord_bot/.env)
make run       # http://127.0.0.1:8080
make smoke     # call /health, /ready, /v1/chat on the running service
make check     # ruff + pytest
```
