# discord_bot

The Discord application (Rust, Serenity + Songbird): slash commands, voice playback, the
server library, and chat forwarded to the [agent runtime](../agent_runtime/). It never calls a
model itself. How playback, radio, uploads, search and the security measures work, and why,
is in the [top-level README](../README.md#how-it-works); this page is about running it.
Code-level design notes live in [`docs/`](docs/README.md).

## Layout

Ports and adapters: `domain/` and `application/` know nothing about Discord, HTTP or
databases; `main.rs` (and `library.rs` for the library) is the only place that wires concrete
adapters.

```text
adapters/inbound/discord/   handler.rs (events), music.rs, library.rs (slash commands),
                            respond.rs (defer-first replies), presenter.rs (all user text)
application/ports/          ChatService, MusicService, LibraryService (inbound)
                            AgentPort, VoicePort, TrackResolver, LibraryRepository,
                            ObjectStore, AudioProcessor, AttachmentFetcher (outbound)
application/use_cases/      ChatUseCase, MusicUseCase (playback rules), LibraryUseCase (upload)
adapters/outbound/          agent_http (runtime client), songbird_voice + icy (playback),
                            audio_http (SSRF-safe client), url_resolver + library_resolver,
                            pg_library (Postgres), s3_store, ffmpeg, attachment_fetch
tls.rs                      process-wide rustls crypto provider (see below)
migrations/, .sqlx/         Postgres schema and the offline query cache
stations.toml               default radio stations (embedded)
```

## Configuration

| Variable | Required | Default | Purpose |
|---|---|---|---|
| `DISCORD_TOKEN` | yes | – | Bot token |
| `AGENT_BASE_URL` | no | `http://localhost:8080` | Agent runtime base URL |
| `AGENT_API_KEY` | no | – | Sent as `Authorization: Bearer <key>` |
| `AGENT_TIMEOUT_SECS` | no | `120` | Chat request timeout |
| `MUSIC_STATIONS_FILE` | no | built-in `stations.toml` | Radio stations (TOML, 1–25 entries) |
| `MUSIC_MAX_QUEUE` | no | `50` | Tracks per server, including the one playing (1–500) |
| `DATABASE_URL` | for the library | – | Postgres (secret). The library is on only with this **and** `MUSIC_S3_BUCKET` |
| `MUSIC_S3_BUCKET` | for the library | – | Bucket for library audio |
| `MUSIC_S3_PREFIX` | no | `library/` | Key prefix for library objects |
| `MUSIC_S3_ENDPOINT` | no | AWS | S3-compatible endpoint (RustFS locally); path-style addressing |
| `MUSIC_S3_CREATE_BUCKET` | no | `false` | Create the bucket at start (local development) |
| `MUSIC_UPLOAD_ROLE` | no | `DJ` | Role name (per server) that may upload and delete others' tracks; Manage Server always may |
| `MUSIC_GUILD_QUOTA_MB` | no | `500` | Library storage per server |
| `DATABASE_MAX_CONNECTIONS` | no | `3` | Pool size (keep small for Supavisor session mode) |
| `FFMPEG_PATH`, `FFPROBE_PATH` | no | on `PATH` | ffmpeg binaries (bundled in the image) |
| `AWS_*` | for the library | ECS task role | S3 credentials and region (default AWS chain) |

Setting only one of `DATABASE_URL` / `MUSIC_S3_BUCKET` is a startup error. With neither, the
bot runs without the library: 6 commands instead of 9, and `/play` accepts links only.

## Discord setup

* Invite with the `bot` and `applications.commands` scopes and the **View Channels**,
  **Send Messages**, **Read Message History**, **Connect** and **Speak** permissions.
* Turn on **Bot → Privileged Gateway Intents → Message Content Intent** (to read replies
  that don't ping the bot); without it the bot fails with "Disallowed intents".
* **Run exactly one bot per token.** A second process receives the same commands and races
  to answer them: symptoms are "Unknown interaction" in the log, "Loading options failed" in
  Discord, and `/stop` answered by the process that isn't playing. On one machine the bot
  prevents this: it takes an exclusive lock on a file named after a hash of the token in the
  temp directory and refuses to start (`AlreadyRunning(<pid>)`) if another process holds it
  (`instance_lock.rs`). ECS runs exactly one task.

## Commands

| Command | Does |
|---|---|
| `/radio <station>` | Plays a curated station, replacing the queue; `/queue` shows the current song |
| `/play <link or title>` | A direct http(s) audio link, or the best match from this server's library (titles autocomplete) |
| `/skip`, `/stop`, `/queue`, `/leave` | Skip; stop and clear (stays in the channel); show the queue; leave |
| `/upload <file> [title]` | Adds an audio attachment (≤ 25 MB, ≤ 15 min) to the library |
| `/library [page]` | Lists the library, 10 per page, with storage used / quota |
| `/delete <track>` | Removes a track (uploader or upload role) |

The last three exist only with the library on. Chat: mention the bot or reply to one of its
messages; the mention is stripped, a typing indicator shows, and the answer is split at
Discord's 2,000 characters. Bots (including itself) are ignored.

**Music from chat.** In a server, every chat request also offers the configured stations as
a client-side tool. "@TatarBot put on some jazz", "เปิดเพลงชิลๆ หน่อย", "skip this" or "what's
playing?" come back as actions, which `chat_actions.rs` carries out through `MusicService`
(at most 2 per message): play a station, play a song from this server's library by title
("@TatarBot เปิดเพลง Boys Dont Cry หน่อย"; the 20 most recent titles are sent as hints),
skip, stop, or show the queue. A title goes through the same search as `/play <title>`;
anything link-like is refused before that. Leaving, links, uploads and deletes stay
slash-command only. If an action fails, the bot posts the real error
instead of the agent's text. Not offered in DMs (no voice there). Each chat request also
sends what is playing now and the outcomes of the actions carried out after the
conversation's previous message, so the agent knows what it started (`chat_actions.rs`,
`PendingReports`).

**Stations.** The defaults are SomaFM channels (`lofi`, `jazz`, `chill`, `ambient`,
`lounge`), which serve a descriptive User-Agent (the bot sends `TatarBot/<version> (+repo)`)
but not generic clients. SomaFM is listener-supported: check its terms before using it in a
public server, or set `MUSIC_STATIONS_FILE`.

## Chat: runtime contract and failures

```http
POST {AGENT_BASE_URL}/v1/chat
Authorization: Bearer <AGENT_API_KEY>
{ "message": "hello", "conversation_id": "discord:<guild|dm>:<channel>:<user>",
  "client_tools": { "music": { "stations": [ { "name": "lofi", "title": "..." } ] } } }

200 { "message": "hi!", "conversation_id": "...",
      "actions": [ { "type": "play_music", "action": "play", "station": "lofi" } ] }
4xx/5xx { "error": "reason" }
```

`client_tools` is sent only in servers; `actions` is optional (older runtimes omit it) and
unknown actions are ignored.

| What fails | Runtime returns | Discord user sees |
|---|---|---|
| Runtime unreachable | – | "⚠️ I can't reach my brain right now…" |
| Wrong `AGENT_API_KEY` | 401 | same; the bot logs an operator error |
| Model unreachable, 5xx, 429 | 503 | same |
| Model rejected the request, or an invalid reply | 502 | same |
| Model slower than the runtime's timeout | 504 | "⌛ That took too long to answer…" |
| Runtime slower than `AGENT_TIMEOUT_SECS` | – | same |

Upstream bodies, URLs and keys never reach Discord.

## Operations

* **Library without its database** (e.g. a paused Supabase project): the bot starts and keeps
  playing radio and links; library commands answer "temporarily unavailable"; a supervisor
  reconnects with backoff up to 5 minutes and runs migrations on connect. After 10 minutes it
  logs a hint about paused Supabase projects, which need "Resume project" in the dashboard.
* **Supabase** connection (Supavisor pooler, session mode, verified TLS; the root CA is in
  the image):
  `postgres://postgres.<ref>:<password>@aws-1-ap-southeast-1.pooler.supabase.com:5432/postgres?sslmode=verify-full&sslrootcert=/etc/tatarbot/supabase-root-2021.crt`
* On connect the bot logs whether Thai titles get fuzzy matching or substring matching only
  (it depends on the database's `LC_CTYPE`).
* **TLS crypto provider:** dependencies compile in both rustls providers, and Songbird's voice
  WebSocket panics unless one is installed process-wide. `tls.rs` installs aws-lc-rs at
  startup and checks it; a test drives the same `wss://` path.
* **Startup:** if Discord is unreachable the bot retries with backoff (a rejected token fails
  at once). On SIGTERM it leaves voice channels and closes the gateway, at most 5 s each.
* Slash commands are registered globally at startup (a bulk overwrite).

## Development

```sh
make env            # .env from .env.example; fill in DISCORD_TOKEN
make run            # debug build with .env
make dev            # bot + an echo mock of the runtime on :8080
make check          # fmt, clippy -D warnings, unit tests (no database needed)
make library-up     # Postgres + RustFS from ../compose.yaml
make test-db        # integration: Postgres races and search, RustFS, ffmpeg, end to end
make sqlx-prepare   # after changing SQL: migrate + refresh .sqlx (commit it)
make sqlx-check     # fail if .sqlx is stale (CI)
cargo test -- --ignored stations_decode icy_live   # live stations (needs internet)
```

Queries compile from the committed `.sqlx/` cache (`.cargo/config.toml` sets
`SQLX_OFFLINE=true`), so builds never contact a database.

**Acknowledgment timing.** Every slash command is deferred first (`respond.rs`). Each defer
logs `discord_to_bot_ms` (gateway delivery) and `defer_ms` (the ack round trip): at debug
level under `tatarbot::timing`, as a warning above 2 s, and in the warning when a defer
fails. Small times with "Unknown interaction" mean another process answered first.

**Measuring latency.** Run the release build with
`RUST_LOG=info,serenity=warn,tatarbot::timing=debug NO_COLOR=1`, save the log, use the bot,
then `python3 scripts/timing_report.py bot.log`. It reports command to first audio (Discord
to bot, handling, queued to playable), voice join, search and autocomplete times.

## Container

```sh
make image       # docker build -t tatarbot-discord:local .
make image-run   # with .env; a runtime on this machine is http://host.docker.internal:8080
```

A release build in a distroless image (glibc + CA certificates, no shell), non-root, with
static libopus, a minimal static ffmpeg/ffprobe (9.0.2, source hash pinned) and Supabase's
root CA. No inbound port; voice is outbound UDP. With the library, `/tmp` must be writable
(ECS mounts a scratch volume; the root filesystem stays read-only). In Compose the runtime
is `http://agent_runtime:8080`; in ECS `http://runtime.tatarbot.internal:8080`.
