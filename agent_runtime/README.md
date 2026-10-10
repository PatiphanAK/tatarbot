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
application/chat_service.py       ChatService: history, language, guardrails, sources
        │
        ▼
application/agent.py              SearchAgent: ReAct loop  reason → act ⇄ observe
        │            │                 │
        ▼            ▼                 ▼
 ConversationStore  ModelPort      SearchPort    application/ports.py (abstract)
        │            │                 │
        ▼            ▼                 ▼
 memory_store.py    vllm.py        tavily.py     adapters/outbound/
```

* `domain/inference.py` defines the generic contract: `InferenceRequest` (messages + params),
  `InferenceResult`, and `ChatMessage`. It contains no model name, URL, HTTP or Discord types.
* `ChatService` depends only on the two ports. Possible replacements:
  * `ModelPort`: `VLLMModel` today, later `OllamaModel`, `OpenAIModel`, or a queue/scheduler
    adapter that hands `InferenceRequest`s to GPU workers.
  * `ConversationStore`: in-memory today, Redis/Postgres later.
  * HTTP inbound: could be joined by an event/queue consumer that calls the same `ChatService`.
* `domain/language.py` and `domain/guardrails.py` are pure policies `ChatService` applies
  around the model call (see below).
* `main.py` (`create_app`) is the composition root.

## Web search agent (ReAct)

With `TAVILY_API_KEY` set, each turn runs a ReAct loop (Thought → Action → Observation),
built as a LangGraph graph in `application/agent.py`:

```text
START → reason ──▶ act ──web_search──▶ observe ─┐
          ▲                                      │
          └──────────────────────────────────────┘
                   act ──answer──▶ END
```

* **reason (Thought)**: a short model call (≤ 256 tokens, no tools). The model writes, in
  English, what the user wants, what it already knows and what is missing, and ends with
  `Next: SEARCH <query>` or `Next: ANSWER`. It sees earlier thoughts and the remaining
  budget (`SEARCH_MAX_SEARCHES`, default 3 per message).
* **act (Action)**: carries out the thought. `web_search` is offered only when the thought
  chose SEARCH (or was unclear) and budget remains; after `Next: ANSWER` the model must
  answer. If it still tries to call a tool, it is asked once more to answer.
* **observe (Observation)**: runs the searches concurrently through `SearchPort` (Tavily,
  5 results, excerpts cut to 800 characters), adds them as tool messages marked untrusted,
  and loops back to reason.

Thoughts are internal. They are logged (`react thought ... decision=search query=...`) and
returned in `AgentResult.steps`, but never sent to Discord or stored in history; any
`Thought:`/`Next:` lines a model echoes at the start of its answer are removed. A turn
log line reports `react_steps`, `searches` and `model_calls`.

Cost: every search-enabled turn makes at least two model calls (reason + act), so expect
roughly 1.5–2× latency per turn compared with plain chat, more when it searches.

* The system message gains search rules and today's date.
* The reply gets a `Sources:` / `แหล่งที่มา:` footer with up to 3 links taken from the actual
  search results (not from the model), as `<url>` so Discord does not unfurl them.
* History stores only the user message and the final reply: no thoughts, tool messages or
  footer.
* The whole turn is bounded by `TURN_TIMEOUT_SECONDS` (110 s, below the bot's 120 s) → 504.
  Model errors keep their usual mapping; a failed search becomes an observation and the
  model answers without it.

The graph calls the same `ModelPort` as before (no LangChain model wrapper), so error
mapping, guardrails and tests are unchanged. Without the key there is no reasoning step and
no `tools`: one plain model call, exactly as before. The model server must support tool
calling: see `model-inference/` (vLLM `--enable-auto-tool-choice --tool-call-parser hermes`).

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
