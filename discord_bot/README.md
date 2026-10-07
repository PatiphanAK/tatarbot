# TatarBot – Discord adapter

TatarBot is the Discord application. It does **not** run or call LLMs. It forwards chat to the
Agent Runtime, which talks to the Tatar Gateway or inference backend (vLLM, Ollama, …).

```text
Discord ──(Gateway WebSocket)──▶ TatarBot ──(AgentPort / HTTP)──▶ Agent Runtime ──(ModelPort)──▶ Tatar Gateway / inference
```

Inside TatarBot (hexagonal / ports and adapters):

```text
adapters/inbound/discord        Discord adapter (poise/serenity)
        │
        ▼
ChatService                     inbound port
        │
ChatUseCase                     application
        │
        ▼
AgentPort                       outbound port
        │
        ▼
adapters/outbound/agent_http    Agent HTTP adapter (reqwest) ──▶ Agent Runtime
```

* `domain/` and `application/` have no dependency on Discord, HTTP or any model provider.
* `main.rs` is the composition root. It is the only place that wires concrete adapters together.
* TatarBot has no knowledge of model names, providers or GPU infrastructure. Those belong
  to the Agent Runtime.

## Configuration

| Variable | Required | Default | Purpose |
|---|---|---|---|
| `DISCORD_TOKEN` | yes | – | Bot token |
| `DISCORD_GUILD_ID` | no | – | Register slash commands in one guild (instant) |
| `DISCORD_RESPOND_TO_MENTIONS` | no | `true` | Also answer @mentions and DMs |
| `AGENT_BASE_URL` | no | `http://localhost:8080` | Agent Runtime base URL |
| `AGENT_API_KEY` | no | – | Sent as `Authorization: Bearer <key>` |
| `AGENT_TIMEOUT_SECS` | no | `120` | Request timeout |

## Agent Runtime contract

```http
POST {AGENT_BASE_URL}/v1/chat
Authorization: Bearer <AGENT_API_KEY>          # only if configured
{ "message": "hello", "conversation_id": "discord:<guild|dm>:<channel>:<user>" }

200 { "message": "hi!", "conversation_id": "..." }
4xx/5xx { "error": "reason" }   # body optional
```

The runtime owns conversation history, keyed by `conversation_id`.

## Running

```sh
make env      # creates .env from .env.example; fill in DISCORD_TOKEN
make dev      # bot + echo mock Agent Runtime on :8080 (uses AGENT_API_KEY if set)
make run      # bot against AGENT_BASE_URL
make check    # fmt check + clippy + tests
make help     # all targets
```

Invite the bot with the `bot` and `applications.commands` scopes, plus the Send Messages and
Read Message History permissions. No privileged intents are needed: Discord delivers message
content for DMs and for messages that mention the bot.

Usage: `/chat message:<text>`, or mention the bot or DM it.
