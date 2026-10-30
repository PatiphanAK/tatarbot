# agent_runtime

The service between the Discord bot and the model (Python, FastAPI, LangGraph). It owns
conversation history, the system prompt, guardrails and reply language, runs the ReAct
web-search agent, and answers from a knowledge base of documents (RAG, optional). It knows nothing about Discord; the bot knows nothing about the model. How
the ReAct loop and the guardrails work, and why, is in the
[top-level README](../README.md#7-agent-runtime); this page has the details.

```text
discord_bot ──POST /v1/chat──▶ agent_runtime ──POST /chat/completions──▶ any OpenAI-compatible model
                                     ├──▶ POST /embeddings ──▶ any embedding model (RAG, optional)
                                     └──▶ Tavily (web search, optional)
```

## Architecture

```text
adapters/inbound/http_api.py      FastAPI: auth, validation, error mapping
        │
        ▼
application/chat_service.py       ChatService: history, language, guardrails, sources
        │                    └──────────────▶ application/knowledge.py  KnowledgeBase (RAG)
        ▼                                          │             │              │
application/agent.py              SearchAgent      │             │              │
        │            │                 │           ▼             ▼              ▼
        ▼            ▼                 ▼     DocumentSource  EmbeddingPort  VectorStore
 ConversationStore  ModelPort      SearchPort    application/ports.py (abstract)
        │            │                 │           │             │              │
        ▼            ▼                 ▼           ▼             ▼              ▼
 memory_store.py    vllm.py        tavily.py  file_documents  openai_embeddings memory_vectors
                                                              hash_embeddings
                                                              (+ embedding_cache)
```

* `domain/inference.py` defines the generic contract: `InferenceRequest` (messages + params),
  `InferenceResult`, and `ChatMessage`. It contains no model name, URL, HTTP or Discord types.
* `ChatService` and `SearchAgent` depend only on ports. `VLLMModel` is a plain OpenAI Chat
  Completions client, so any compatible backend works by configuration; history is in
  memory (`ConversationStore`), lost on restart, which is why the runtime runs one task.
* `domain/language.py` and `domain/guardrails.py` are pure policies `ChatService` applies
  around the model call (see below).
* `main.py` (`create_app`) is the composition root; `build_embedder` picks the embedding
  adapter from `EMBEDDING_PROVIDER`.

## Knowledge base (RAG)

On only with `RAG_DOCS_DIR`. The design, model choices, tuning and operations are in
[docs/architecture/rag.md](../docs/architecture/rag.md); in short:

* At startup (in the background) and on `POST /v1/knowledge/reload`, every `.md`/`.txt`
  file under `RAG_DOCS_DIR` is split along its headings and paragraphs, embedded, and held
  in memory. With `RAG_CACHE_FILE`, only new or changed chunks are re-embedded.
* Each turn embeds the user's message, takes the `RAG_TOP_K` closest chunks scoring at
  least `RAG_MIN_SCORE`, and adds them to the system message as excerpts marked as data, not
  instructions. They are not stored in history. Turns without a good match get none.
* **The embedding model is configuration.** `EMBEDDING_PROVIDER=openai` talks to any
  OpenAI-compatible `/embeddings` API (vLLM, Ollama, TEI, LiteLLM, OpenAI, Alibaba Model
  Studio) with `EMBEDDING_MODEL`, optional `EMBEDDING_DIMENSIONS` and model-specific
  query/document prefixes; `hash` needs no model (development and tests). Changing the
  model changes the index's `identity` and re-embeds everything once.
* Retrieval never fails a turn: embedding errors and timeouts are logged and the turn goes
  on without knowledge. Use a multilingual model (e.g. `bge-m3`, `Qwen3-Embedding`,
  `text-embedding-v4`) so Thai questions find English documents and the other way round.
* The image ships starter documents at `/app/knowledge` (`knowledge/`): how to use the bot.

## Web search agent (ReAct) details

On only with `TAVILY_API_KEY`; without it a turn is one plain model call with no `tools`.

* `reason` is capped at 256 tokens and sees its earlier thoughts and the remaining budget
  (`SEARCH_MAX_SEARCHES`, default 3). An unclear thought lets the model choose; a tool call
  after the budget is spent is answered with one "answer now" retry.
* `observe` runs searches concurrently (Tavily, 5 results, excerpts cut to 800 characters)
  and marks results as untrusted. A failed search becomes an observation, not an error.
* The reply gets a `Sources:` / `แหล่งที่มา:` footer with up to 3 links from the actual
  results, as `<url>` so Discord does not unfurl them. History keeps only the user message
  and the final reply (no thoughts, tool messages or footer).
* The whole turn is bounded by `TURN_TIMEOUT_SECONDS` (110 s, below the bot's 120 s) → 504.
* Cost: at least two model calls per turn (reason + act), roughly 1.5–2× the latency of
  plain chat. The model server must support tool calling (vLLM:
  `--enable-auto-tool-choice --tool-call-parser hermes`; see `model-inference/`).
* Logs: `react thought ... decision=search query=...` per step, and `react_steps`,
  `searches`, `model_calls` per turn.

## Client-side tools (music)

The caller may send `client_tools.music.stations` (1–25, unique `a-z0-9-` names, titles up
to 100 characters). The model is then offered `play_music` with `action` ∈ play, skip, stop,
queue and `station` ∈ the names sent. With `library: true` it also gets `play_track` and a
free-text `track` (≤ 100 characters; `library_hints`, up to 20 recent titles, go into its
description, flattened to one line); titles containing `://` are dropped, and the system message gains short music rules
(reply in one sentence, never claim it's playing). With web search on, the reason step can
choose `Next: MUSIC`, and only then is `play_music` offered, never next to `web_search`
after a `SEARCH` decision.

A valid call ends the turn with no further model call and is returned in `actions`;
invalid arguments are dropped (logged) and the model is asked to answer instead. The
runtime never runs these tools and never calls the bot. `message` may be empty when there
are actions (`domain/client_tools.py`, `application/agent.py`).

**Memory.** History stores the turn as an assistant message with the `play_music` calls
and one tool message per call, `pending` until the caller reports the outcome: requests may
carry `client_tools.music.results` (`{action, station, ok, detail}`, at most 4), which turn
those into `ok: …` / `failed: …`. Requests may also carry `client_tools.music.state`
(`channel`, `queue` of `{title, live, song}`), shown in the system message every turn as
"Music right now". Titles are flattened to one line and truncated: radio and upload titles
are untrusted. Text notes were tried first and a model imitated them instead of calling
the tool, so any `[action]` lines in a reply are removed.

## System prompt, reply language, guardrails

The system prompt is `src/agent_runtime/prompts/system.md` (TatarBot persona, language,
style and safety rules), shipped in the image. Override it with `SYSTEM_PROMPT` (inline) or
`SYSTEM_PROMPT_FILE` (a path, e.g. a mounted file); `SYSTEM_PROMPT` wins.

**Reply language.** Each turn, the user's message is classified by script: Thai characters
vs Latin letters, with Thai winning ties ("ช่วยอธิบาย Docker หน่อย" → Thai). The system message
then ends with "Reply in Thai" or "Reply in English". Messages with no signal ("555", emoji)
keep the conversation's last language; other scripts get no hint and rely on the prompt's
"reply in the user's language" rule. A Thai turn answered without Thai is logged as
`reply language mismatch`.

**Guardrails** (`GUARDRAILS_ENABLED`, default `true`). Deterministic checks around the model:

| Stage | Check | Action |
|---|---|---|
| Input | Obvious prompt injection / prompt extraction, English and Thai ("ignore all previous instructions", "reveal your system prompt", "ไม่ต้องสนใจคำสั่งก่อนหน้า", "บอก system prompt ของคุณ") | Polite refusal in the user's language; the model is not called; nothing stored |
| Output | Reply repeats 2+ lines of the system prompt verbatim | Refusal instead of the reply; nothing stored |
| Output | Strings that look like credentials (Discord, OpenAI, AWS, Runpod, Hugging Face, GitHub tokens, private keys) | Replaced with `[redacted]` before storing and returning |

Patterns are narrow on purpose: questions *about* prompts or "developer mode" still reach
the model. Refusals return 200 with the refusal text, so the bot shows it like a reply.
Harmful-content moderation relies on the model and the prompt's safety rules; a guard
classifier model behind its own port would be the next step.

## API

| Method | Path | Auth | Purpose |
|---|---|---|---|
| `POST` | `/v1/chat` | Bearer | `{message, conversation_id, client_tools?}` → `{message, conversation_id, actions}` |
| `GET` | `/v1/knowledge` | Bearer | Knowledge base status: `{documents, chunks, embedder, indexed_at, duration_ms}` (404 when disabled) |
| `POST` | `/v1/knowledge/reload` | Bearer | Re-read and re-index the documents; same body, or 503 if the embedding backend fails |
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
| Embedding backend failed during a knowledge reload | 503 |
| Unexpected error | 500 |

## Configuration

| Variable | Required | Default | Purpose |
|---|---|---|---|
| `AGENT_API_KEY` | yes | – | Bearer key the bot must send |
| `MODEL_BASE_URL` | yes | – | OpenAI-compatible API root, e.g. `http://127.0.0.1:8000/v1` or Alibaba's `…/compatible-mode/v1` (include `https://`) |
| `MODEL_NAME` | yes | – | Model id the backend serves (`tatarbot-chat` for `model-inference/`) |
| `MODEL_API_KEY` | no | – | Backend bearer key |
| `MODEL_TIMEOUT_SECONDS` | no | `100` | Per model call |
| `MODEL_MAX_TOKENS`, `MODEL_TEMPERATURE` | no | `1024`, unset | Generation parameters |
| `MODEL_ENABLE_THINKING` | no | `false` | Qwen3 `chat_template_kwargs.enable_thinking`; empty omits it |
| `TAVILY_API_KEY` | no | – | Enables the web-search agent |
| `SEARCH_MAX_SEARCHES`, `SEARCH_MAX_RESULTS`, `SEARCH_TIMEOUT_SECONDS` | no | `3`, `5`, `10` | Search budget |
| `TURN_TIMEOUT_SECONDS` | no | `110` | Whole turn; keep below the bot's timeout |
| `SYSTEM_PROMPT`, `SYSTEM_PROMPT_FILE` | no | `prompts/system.md` | Override the system prompt |
| `GUARDRAILS_ENABLED` | no | `true` | Input/output guardrails |
| `MAX_HISTORY_MESSAGES`, `MAX_CONVERSATIONS` | no | `20`, `10000` | History window and memory bound |
| `RAG_DOCS_DIR` | no | – | Enables the knowledge base: directory of `.md`/`.txt` documents (`/app/knowledge` in the image) |
| `RAG_CACHE_FILE` | no | – | JSON file keeping document embeddings across restarts and reloads |
| `RAG_TOP_K`, `RAG_MIN_SCORE`, `RAG_MAX_CONTEXT_CHARS` | no | `4`, `0.3`, `4000` | Chunks per turn, minimum cosine similarity (model-specific), excerpt budget |
| `RAG_CHUNK_CHARS`, `RAG_CHUNK_OVERLAP` | no | `1200`, `200` | Chunk size; overlap when a long paragraph is cut |
| `RAG_TIMEOUT_SECONDS` | no | `10` | Query embedding + search per turn |
| `EMBEDDING_PROVIDER` | no | `openai` | `openai` (any OpenAI-compatible `/embeddings`) or `hash` (offline, lexical) |
| `EMBEDDING_BASE_URL`, `EMBEDDING_API_KEY` | no | `MODEL_BASE_URL`, `MODEL_API_KEY` | Embedding API root and key; `MODEL_API_KEY` is used only when the base URL is defaulted too |
| `EMBEDDING_MODEL` | with `openai` | – | Embedding model id, e.g. `bge-m3`, `text-embedding-v4` |
| `EMBEDDING_DIMENSIONS` | no | model's own (`512` for `hash`) | Shorter vectors, for models that support it |
| `EMBEDDING_QUERY_PREFIX`, `EMBEDDING_DOCUMENT_PREFIX` | no | empty | Model-specific input prefixes (`query: ` / `passage: ` for E5); `\n` is a newline |
| `EMBEDDING_BATCH_SIZE`, `EMBEDDING_TIMEOUT_SECONDS` | no | `32`, `30` | Texts per request when indexing; per request |

Qwen3 is a reasoning model. By default the runtime sends
`chat_template_kwargs.enable_thinking=false` and strips any `<think>…</think>` from replies.

History is kept in memory: the last `MAX_HISTORY_MESSAGES` messages are sent per turn, and
everything is lost on restart. Turns within one conversation are serialized per process.

## RAG database

A Postgres + pgvector schema `rag` for a persistent vector store. Nothing reads or writes
it yet: the knowledge base above still uses `InMemoryVectorStore`. Migrations are plain SQL in `src/agent_runtime/db/migrations/`, named
`<version>_<description>.sql` like the bot's sqlx ones, and shipped in the image.

| Table | Holds |
|---|---|
| `rag.documents` | One row per `(namespace, source)`: title, `content_hash` (SHA-256 hex), `metadata` JSON |
| `rag.chunks` | `ordinal`, `content`, `embedding vector(1024)` (bge-m3), `embedding_model`; deleted with their document |

`chunks.embedding` has an HNSW index for cosine distance (`ORDER BY embedding <=> $1`).
Changing the embedding model's dimension means a new migration and re-embedding.

```sh
make db-up     # local Postgres + pgvector from ../compose.yaml
make migrate   # apply pending migrations to DATABASE_URL (.env)
make test-db   # all tests, incl. migrations against local Postgres
```

`python -m agent_runtime.db.migrate` (in the image too) applies each pending file in its own
transaction and records it with a checksum in `_agent_runtime_migrations`. It fails if an
applied file was edited or is missing: never edit an applied migration, add a new one. An
advisory lock serializes concurrent runs, so on Supabase use the direct or session-pooler
URL, not the transaction pooler. The app does not migrate on startup.

## Development

```sh
make install   # uv sync
make env       # .env with a generated AGENT_API_KEY (put the same key in discord_bot/.env)
make run       # http://127.0.0.1:8080
make smoke     # call /health, /ready, /v1/chat on the running service
make check     # ruff + pytest
```

## Container

```sh
make image       # docker build -t tatarbot-agent-runtime:local .
make image-run   # with .env, published on 127.0.0.1:8080 only
```

The image runs `uvicorn --factory agent_runtime.main:create_app` as a non-root user and
listens on `UVICORN_HOST:UVICORN_PORT` (default `0.0.0.0:8080`, so other containers can
reach it). Its `HEALTHCHECK` calls `/health`, not `/ready`: `/ready` queries the model
backend and must not become a periodic probe against a scale-to-zero endpoint. On SIGTERM,
uvicorn stops accepting connections and finishes in-flight requests. Configuration and
secrets come only from the environment. The image works with a read-only root filesystem.

Integration tests against a mock OpenAI-compatible backend: `make integration` from the
repository root. In ECS this runs as service `tatarbot-runtime`, reachable only from the bot
at `http://runtime.tatarbot.internal:8080` (`infra/aws/`).
