# TatarBot – Discord adapter

TatarBot is the Discord application. It does **not** run or call LLMs. It forwards chat to the
Agent Runtime, which talks to an OpenAI-compatible inference backend (vLLM, Runpod, …). It also
plays music in voice channels, independently of the runtime.

```text
Discord ──(Gateway WebSocket)──▶ TatarBot ──(AgentPort / HTTP)──▶ Agent Runtime ──(ModelPort)──▶ inference backend
              voice (UDP) ◀──────┘   └──(HTTP)──▶ radio streams / audio links
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

Music has the same shape, with its own ports:

```text
adapters/inbound/discord/music.rs   slash commands (thin)
        │
        ▼
MusicService                        inbound port: play, play_station, skip, stop, leave, queue
        │
MusicUseCase                        all playback rules and validation
        │                 │
        ▼                 ▼
TrackResolver           VoicePort   outbound ports
        │                 │
        ▼                 ▼
url_resolver.rs    songbird_voice.rs ──▶ Discord voice (Songbird, DAVE E2EE)
                          │
                   audio_http.rs     SSRF-safe HTTP client for streams
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
| `MUSIC_STATIONS_FILE` | no | built-in `stations.toml` | Radio stations for `/radio` (TOML, 1–25 entries) |
| `MUSIC_MAX_QUEUE` | no | `50` | Tracks per server, including the one playing (1–500) |

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

Invite the bot with the `bot` and `applications.commands` scopes and the **View Channels**,
**Send Messages**, **Read Message History**, **Connect** and **Speak** permissions.

In the Developer Portal, turn on **Bot → Privileged Gateway Intents → Message Content Intent**.
It is required to read replies to the bot that don't ping it. Without it, the bot fails to
start with "Disallowed intents".

## Usage

Chat works through `MESSAGE_CREATE` events (mentions and replies); music uses slash
commands (see below).

* `@TatarBot what is Rust?`: the bot responds to messages that mention it.
* Replying to one of the bot's messages also works, with or without the reply ping.
* Normal chat that doesn't mention the bot is ignored, and so are messages from bots
  (including itself), so it can't loop.

The mention is stripped before the text is sent to the agent. A typing indicator shows
while the agent works, and the answer is posted as a normal channel message (not a reply),
split at Discord's 2000-character limit.

## Music

Join a voice channel, then:

| Command | Does |
|---|---|
| `/radio <station>` | Plays a curated station (choices from `stations.toml`), replacing the queue |
| `/play <link>` | Plays a direct http(s) audio link (mp3, aac/m4a, ogg/vorbis, flac, wav, or a stream URL), or queues it |
| `/skip` | Skips the current track |
| `/stop` | Stops and clears the queue; the bot stays in the channel |
| `/queue` | Shows what's playing and the next 10 |
| `/leave` | Stops and leaves the voice channel |

Rules, all in `MusicUseCase` so every caller (slash commands now, chat control later) gets
the same behaviour:

* Playing needs you in a voice channel. The bot joins it, or moves there when idle; it won't
  leave listeners in another channel mid-track.
* `/skip`, `/stop` and `/leave` need you in the bot's channel.
* A live station never ends, so anything queued behind it replaces it.
* Searching by title (`/play some song`) arrives with the server library (Phase 1.5); for
  now it explains that and suggests a link or `/radio`.
* Results are public; refusals are only shown to you. Titles from links are
  markdown-escaped and nothing pings anyone.

**Security.** Anyone can make the bot fetch a URL, so `audio_http.rs` resolves every host
through a resolver that drops private, loopback, link-local (incl. the ECS credentials
endpoint `169.254.170.2`), CGNAT and other non-public addresses, and re-checks every redirect.
IP-literal and `localhost` links are refused at `/play`.

**Stations.** The defaults are SomaFM channels (`lofi`, `jazz`, `chill`, `ambient`, `lounge`).
SomaFM streams to a descriptive User-Agent (the bot sends `TatarBot/<version> (+repo URL)`)
but not to generic clients. It is listener-supported; check its terms before using it in a
public server, or point `MUSIC_STATIONS_FILE` at your own list.

`cargo test -- --ignored stations_decode` checks that every default station opens and decodes
through the real voice pipeline (needs internet).

Audio is decoded by Symphonia (pure Rust) and encoded to Opus by libopus, linked statically:
no ffmpeg, no YouTube. Each playing server costs some CPU for Opus encoding; the default
0.25 vCPU task handles a few servers at once.

On startup the bot registers these slash commands globally (replacing the `/chat` command
of earlier versions) and removes leftover per-server commands.

## Container

```sh
make image       # docker build -t tatarbot-discord:local .
make image-run   # with .env; a runtime on this machine is http://host.docker.internal:8080
```

Release Rust build in a distroless image (glibc + CA certificates, no shell), non-root. The
bot needs no inbound port; voice uses outbound UDP only. If Discord is unreachable at start
it retries with backoff (a rejected token still fails at once). It stops on Ctrl-C or SIGTERM
(`docker stop`, ECS) by leaving voice channels and closing the gateway connection, waiting at
most 5 s for each. `DISCORD_TOKEN` and `AGENT_API_KEY` are read from
the environment at start; nothing is baked into the image.

In Compose, `AGENT_BASE_URL` is `http://agent_runtime:8080`; in ECS it is
`http://runtime.tatarbot.internal:8080` (Cloud Map private DNS, `infra/aws/`). Run exactly
one bot per token: two running bots both reply to every message.
