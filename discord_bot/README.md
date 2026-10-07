# TatarBot – Discord adapter

TatarBot is the Discord application. It does **not** run or call LLMs. It forwards chat to the
Agent Runtime, which talks to the Tatar Gateway or inference backend (vLLM, Ollama, …).

```text
Discord ──(Gateway WebSocket)──▶ TatarBot ──(AgentPort / HTTP)──▶ Agent Runtime ──(ModelPort)──▶ Tatar Gateway / inference
```

Inside TatarBot (hexagonal / ports and adapters):

```text
adapters/inbound/discord        Discord adapter (serenity, MESSAGE_CREATE)
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

Invite the bot with the `bot` scope and the **View Channels**, **Send Messages** and
**Read Message History** permissions.

In the Developer Portal, turn on **Bot → Privileged Gateway Intents → Message Content Intent**.
It is required to read replies to the bot that don't ping it. Without it, the bot fails to
start with "Disallowed intents".

## Usage

The bot reacts to `MESSAGE_CREATE` events only. There are no slash commands.

* `@TatarBot what is Rust?`: the bot responds to messages that mention it.
* Replying to one of the bot's messages also works, with or without the reply ping.
* Normal chat that doesn't mention the bot is ignored, and so are messages from bots
  (including itself), so it can't loop.

The mention is stripped before the text is sent to the agent. A typing indicator shows
while the agent works, and the answer is posted as a normal channel message (not a reply),
split at Discord's 2000-character limit.

On startup the bot deletes the `/chat` slash command registered by earlier versions.
