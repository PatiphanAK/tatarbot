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
| `DATABASE_URL` | for the library | – | Postgres for library metadata (secret). Library is on only with this **and** `MUSIC_S3_BUCKET` |
| `MUSIC_S3_BUCKET` | for the library | – | Bucket for library audio |
| `MUSIC_S3_PREFIX` | no | `library/` | Key prefix for library objects |
| `MUSIC_S3_ENDPOINT` | no | AWS | S3-compatible endpoint (RustFS locally); path-style addressing |
| `MUSIC_S3_CREATE_BUCKET` | no | `false` | Create the bucket at start (local development) |
| `MUSIC_UPLOAD_ROLE` | no | `DJ` | Role name (per server) allowed to upload and delete others' tracks; Manage Server always is |
| `MUSIC_GUILD_QUOTA_MB` | no | `500` | Library storage per server |
| `DATABASE_MAX_CONNECTIONS` | no | `3` | Pool size (keep small for Supavisor session mode) |
| `FFMPEG_PATH`, `FFPROBE_PATH` | no | on `PATH` | ffmpeg binaries (bundled in the image) |
| `AWS_*` | for the library | ECS task role | S3 credentials/region (default AWS chain) |

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
* **Radio reconnect:** a station that ends or errors without `/skip`, `/stop` or `/leave` has
  dropped. It is started again as a fresh track (new connection and decoder, a 1–2 s gap)
  after 1, 2, 4, 8, 16 s, giving up after 5 failures in a row; a minute of good playback
  resets the backoff (`songbird_voice.rs`). Splicing a reconnected byte stream into the
  running decoder was rejected: MP3 frames would be cut and the bit reservoir broken, and
  Songbird ends a track on any decode error. A stalled stream is cut after 30 s without data.
* **Now playing:** stations are requested with `Icy-MetaData: 1`; the interleaved
  metadata blocks are stripped before decoding and the `StreamTitle` is shown in `/queue`
  (`icy.rs`).
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

`cargo test -- --ignored stations_decode icy_live` checks that every default station opens
and decodes as a live ICY stream through the real voice pipeline, and that a title arrives
(needs internet).

Audio is decoded by Symphonia (pure Rust) and encoded to Opus by libopus, linked statically:
no ffmpeg, no YouTube. Each playing server costs some CPU for Opus encoding; the default
0.25 vCPU task handles a few servers at once.

On startup the bot registers these slash commands globally (replacing the `/chat` command
of earlier versions) and removes leftover per-server commands.

## Server library (Phase 1.5)

Each server gets its own library of uploaded audio. Enabled when `DATABASE_URL` and
`MUSIC_S3_BUCKET` are both set (one without the other is a startup error).

| Command | Does |
|---|---|
| `/upload <file> [title]` | Adds an audio attachment (upload role or Manage Server) |
| `/play <title>` | Plays the best match from this server's library; titles autocomplete. A link still plays directly |
| `/library [page]` | Lists tracks, 10 per page, with storage used / quota |
| `/delete <track>` | Removes a track (autocomplete); the uploader or the upload role |

```text
/upload ─▶ size check ─▶ download (Discord CDN only, ≤ 25 MB, SHA-256) ─▶ already here? ─▶
  stored by any server? ── yes ─▶ reuse ──────────────────────────────┐
           └─ no ─▶ ffprobe (audio? ≤ 15 min) ─▶ ffmpeg ─▶ Ogg Opus ───┤
                                                                      ▼
            Postgres tx: lock guild + hash, quota, PUT object, INSERT row, commit
/play title ─▶ search ─▶ queue ─▶ (when it starts) presign 30 min ─▶ stream, Opus passthrough
```

* **Validation:** never by extension. ffprobe must find an audio stream and a duration of at
  most 15 minutes. Both ffprobe and ffmpeg run on local `file:` paths only, with
  `-protocol_whitelist file` and a demuxer whitelist (mp3, ogg, flac, wav, mp4/m4a,
  mkv/webm, aac). Each runs with limits (512 MB address space, CPU seconds, 64 MB output, a
  wall-clock timeout), one transcode at a time. The image's ffmpeg is a minimal static build
  with only those demuxers and the file protocol: HLS, m3u, concat and network inputs aren't
  compiled in. A crafted HLS or concat playlist that makes a stock ffmpeg read another local
  file fails on both counts (tested).
* **Stored once:** objects are keyed by the SHA-256 of the *uploaded original*
  (`library/<hash>.ogg`), so the same file in many servers is one object and is transcoded
  once. Each server's quota counts it in full.
* **Playback:** the object is presigned when the track starts (so a long queue can't outlive
  the URL) and fetched with a separate client. Ogg Opus 48 kHz stereo with 20 ms frames is
  what Discord sends, so Songbird passes it through without re-encoding while it is the only
  track at full volume.
* **Consistency** (`pg_library.rs`): upload and delete take `pg_advisory_xact_lock` on the
  hash and write/delete the object while holding it, so an upload can't attach a row to an
  object being deleted. Upload also locks the server, so concurrent uploads can't both pass
  the quota. An hourly sweep deletes objects with no row that are older than 1 hour, and
  re-checks row count and age under the same lock. A crash between object write and commit
  leaves an orphan that the sweep removes later.
* **Search** (`/play <title>` and autocomplete, `pg_library.rs`): a title is a candidate if it
  contains the query (`LIKE` on the lowercased title, wildcards escaped) or is
  trigram-similar (`pg_trgm` `word_similarity`/`similarity` ≥ 0.3, for typos and partial
  words). Ranked exact > prefix > substring > trigram score, then shorter titles. Thai has
  no spaces and uses combining marks; whether `pg_trgm` sees Thai characters depends on the
  database's `LC_CTYPE`. Under `en_US.UTF-8` Thai typos match too; under `C` Thai yields no
  trigrams and only substring matching works (tested both). The bot logs which applies on
  connect. A GIN trigram index serves both. Autocomplete can't be deferred, so its search
  is capped at 2 s (no suggestions rather than an error).
* **Without the database** (e.g. a paused Supabase project) the bot starts and works: radio
  and links play, library commands answer "temporarily unavailable", and a supervisor
  reconnects with backoff (1 s up to 5 min), running migrations on connect. After 10 minutes
  down it logs a hint about paused Supabase projects. A track already playing keeps playing.

### Postgres: local and Supabase

Locally, `make library-up` starts Postgres and RustFS from `../compose.yaml` (on
`127.0.0.1:55432` and `:59000`). Production is Supabase (free tier, Singapore) through the
**Supavisor pooler in session mode** (port 5432). Transaction mode (6543) breaks sqlx's
prepared statements. Use TLS with verification; the image includes Supabase's public root CA
(the pooler's `*.pooler.supabase.com` certificate verifies against it):

```text
DATABASE_URL=postgres://postgres.<project-ref>:<password>@aws-1-ap-southeast-1.pooler.supabase.com:5432/postgres?sslmode=verify-full&sslrootcert=/etc/tatarbot/supabase-root-2021.crt
```

Pausing: Supabase pauses free projects without "sufficient user database activity over the
past week"; its docs say "a few user requests to the database each day" are enough. The bot
reads the library table every 6 hours as a keep-alive (it also health-checks every minute).
A paused project is **not** resumed by connecting: use "Resume project" in the dashboard.

### Developing the library

```sh
make library-up      # Postgres + RustFS (compose)
make test-db         # integration tests: advisory-lock races, sweep, S3, ffmpeg, end to end
make sqlx-prepare    # after changing SQL: migrate + refresh the .sqlx cache (commit it)
make sqlx-check      # CI: fail if .sqlx is stale
```

Queries are checked at compile time from the committed `.sqlx/` cache
(`.cargo/config.toml` sets `SQLX_OFFLINE=true`), so builds never need, or contact, a
database. Migrations live in `migrations/` and are embedded in the binary. The race tests
were checked by removing each lock: they fail without it.

## Container

```sh
make image       # docker build -t tatarbot-discord:local .
make image-run   # with .env; a runtime on this machine is http://host.docker.internal:8080
```

Release Rust build in a distroless image (glibc + CA certificates, no shell), non-root. The
bot needs no inbound port; voice uses outbound UDP only. With the library, `/tmp` must be
writable (download and transcode); ECS mounts a scratch volume there and keeps the root
filesystem read-only. If Discord is unreachable at start
it retries with backoff (a rejected token still fails at once). It stops on Ctrl-C or SIGTERM
(`docker stop`, ECS) by leaving voice channels and closing the gateway connection, waiting at
most 5 s for each. `DISCORD_TOKEN` and `AGENT_API_KEY` are read from
the environment at start; nothing is baked into the image.

In Compose, `AGENT_BASE_URL` is `http://agent_runtime:8080`; in ECS it is
`http://runtime.tatarbot.internal:8080` (Cloud Map private DNS, `infra/aws/`). Run exactly
one bot per token: two running bots both reply to every message.
