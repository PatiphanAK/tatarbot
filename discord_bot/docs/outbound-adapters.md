# Outbound adapters: Songbird voice, ICY radio, sqlx library

Status: describes the code as of 2026-10-18 (`main` at `2c7a971`).

This is a code-level reference for three outbound adapters in `discord_bot/` (paths below
are relative to it). The [top-level README](../../README.md#how-it-works) explains *what* playback, radio and the
library do for users. This page covers *how* the code does it: the types, the control flow,
the invariants, and where to change things.

| File | Implements | Talks to |
|---|---|---|
| `src/adapters/outbound/songbird_voice.rs` | `VoicePort` | Songbird (voice connection, queue, mixer) |
| `src/adapters/outbound/icy.rs` | Songbird `Compose` input for live stations | Icecast/SHOUTcast over HTTP |
| `src/adapters/outbound/pg_library.rs` | `LibraryRepository` | Postgres via sqlx (Supabase in production) |
| `migrations/*.sql`, `.sqlx/` | schema, offline query cache | – |

Wiring happens in `src/main.rs` (voice) and `src/library.rs` (database). Nothing else
constructs these types.

---

## 1. `songbird_voice.rs`: `SongbirdVoice`

### 1.1 Shape

```text
SongbirdVoice (Clone)            ← what MusicUseCase holds as Arc<dyn VoicePort>
└── Arc<Inner>
    ├── manager: Arc<Songbird>          one Call per guild, built-in queue ("builtin-queue")
    ├── http: reqwest_audio::Client     SSRF-safe client (audio_http::client) for links + radio
    ├── library: Option<Arc<LibraryAudio>>
    │     ├── store: Arc<dyn ObjectStore>   presigns library objects
    │     └── http: trusted_client          no public-address guard (our own bucket)
    └── reconnects: StdMutex<Reconnects>    radio reconnect state, shared by all guilds
```

`Inner` is split out so the event handlers (`TrackEvents`) can hold an `Arc<Inner>` and call
`restart_station` from a spawned task.

`reqwest_audio` is reqwest **0.12**, renamed in `Cargo.toml`. Songbird 0.6's `HttpRequest`
requires a 0.12 client, while the rest of the bot uses reqwest 0.13.

### 1.2 Per-track metadata

Every Songbird track carries `Arc<TrackMeta>` as user data (`SongbirdTrack::new_with_data`).
You read it back with `handle.data::<TrackMeta>()`.

| Field | Use |
|---|---|
| `info: TrackInfo` | title, `live`, requester; returned by `queue()` |
| `source: TrackSource` | `Http(url)` or `Library { key }`; needed to rebuild a station on reconnect |
| `now_playing: Option<Arc<NowPlaying>>` | live tracks only; shared with the `LiveStream` input, which writes ICY titles into it |
| `enqueued_at: Instant` | for the `tatarbot::timing` "first audio ready" log |

`queue()` returns one `TrackInfo` per handle, with `now_playing` filled from the live title.
Songbird's queue is the only source of truth. The adapter keeps no separate list.

### 1.3 Choosing an input (`Inner::songbird_track`)

| `TrackSource` | `info.live` | Songbird input | Opened when |
|---|---|---|---|
| `Http(url)` | `true` | `Input::Lazy(LiveStream)` (`icy.rs`) | track starts |
| `Http(url)` | `false` | `HttpRequest` (Songbird built-in, ranged GETs) | track starts |
| `Library { key }` | – | `Input::Lazy(PresignedObject)` | track starts |

All three are lazy, so queuing costs no network. Queuing a library track when `library` is
`None` fails with `VoiceError::Other("library playback not configured")`.

`PresignedObject` is a `Compose` that calls `store.presign_get(key, PRESIGN_TTL)` (30 min) in
`create_async`, then delegates to `HttpRequest` with the trusted client. The URL is
presigned on the track's turn, so it can't expire while the track waits in a long queue. The
TTL is longer than the longest allowed track (15 min), so seeks and Songbird's ranged
re-requests stay valid.

Library audio is stored as Ogg Opus at 48 kHz stereo. Songbird passes it through without
re-encoding while it is the only track at full volume.

### 1.4 Joining and global events

On a guild's **first** `join` (no `Call` existed yet), three global event handlers are
registered on the `Call`. They persist across tracks, and re-joins don't add duplicates:

| Event | Handler | Purpose |
|---|---|---|
| `TrackEvent::Playable` | `PlayableTiming` | debug log `queued_to_playable_ms` (target `tatarbot::timing`) |
| `TrackEvent::End` | `TrackEvents` | log failures, decide radio reconnect |
| `TrackEvent::Error` | `TrackEvents` | same handler; one track can fire both events |

`join` also self-deafens, because the bot never listens. Deafening tells Discord to stop
sending voice packets to the bot.

### 1.5 Radio reconnect policy (`Reconnects`, pure and unit-tested)

A live track that ends or errors **without** the user ending it means the stream dropped.

```text
TrackEvents::act(End|Error, live track)
  └─ Reconnects::on_live_end(guild, track_uuid, play_time)
       ├─ uuid already settled?               → Ignore   (user skip/stop/leave, or the 2nd of Error+End)
       ├─ play_time ≥ 60 s                    → reset this guild's attempts to 0
       ├─ attempts += 1; attempts > 5         → GiveUp   (and reset to 0, so the next /radio starts fresh)
       └─ otherwise                           → ReconnectAfter(1 s · 2^(attempts-1))   1, 2, 4, 8, 16 s
```

* **Settling.** `skip`, `stop` and `leave` call `mark_user_ended` with the affected track
  UUIDs *before* they end the tracks. The later End event then sees an already-settled UUID
  and is ignored. `settled` is capped at 10,000 entries and cleared when full, because
  nothing tells the adapter which event for a track is the last one.
* **Backoff is per guild**: `attempts: HashMap<GuildId, u32>`.
* **Restart** (`Inner::restart_station`, after the delay in a spawned task) builds a *new*
  track from the same `TrackMeta`, with a new connection and a new decoder. It does nothing
  if the bot has left the channel or the queue is non-empty, because the user queued
  something else in the meantime.

*Why not splice the reconnected byte stream into the running decoder:* MP3 frames depend on
the bit reservoir of previous frames, and Songbird ends a track on any decode error. A fresh
track costs a 1–2 s gap but always decodes.

### 1.6 Other entry points

* `leave_all(&Songbird)` is called on shutdown so the bot doesn't linger in voice until
  Discord times it out.
* `songbird_guild` converts a domain `GuildId(u64)` to Songbird's `NonZeroU64` id. An id of
  0 becomes `None`, which callers treat as "not connected" or "invalid id".

### 1.7 Tests

| Test | Kind |
|---|---|
| `dropped_streams_back_off_then_give_up`, `a_minute_of_playback_resets_the_backoff`, `user_ended_and_duplicate_events_are_ignored`, `backoff_is_per_guild` | unit, pure `Reconnects` |
| `private_hosts_are_refused_at_play_time` | unit: the SSRF guard also applies inside Songbird's fetch |
| `stations_decode` | `#[ignore]`, network: every station in `stations.toml` opens and decodes 20 packets. `cargo test -- --ignored stations_decode` |

---

## 2. `icy.rs`: live stations with now-playing titles

### 2.1 The ICY protocol in brief

Request with `Icy-MetaData: 1`. If the server supports it, it replies with an
`icy-metaint: N` header and then interleaves metadata into the body:

```text
[ N audio bytes ][ L ][ L·16 bytes metadata, NUL-padded ][ N audio bytes ][ 0 ][ N audio bytes ] ...
                  ^ one length byte; 0 = no change
metadata: StreamTitle='Artist - Title';StreamUrl='...';
```

Those metadata bytes **must never reach the decoder**, or decoding fails. A server that
sends no `icy-metaint` (or sends 0 or an unparsable value) gets its body passed through
untouched.

### 2.2 Pipeline

```text
LiveStream::create_async            (Songbird Compose; runs when the track starts)
  GET url, Icy-MetaData: 1  ── audio_http::client (SSRF-safe, 30 s read timeout)
  parse icy-metaint
  └─ IcySource
       response.bytes_stream()
         .map(IcyParser::feed)      → (audio bytes, Option<title>)
             title → NowPlaying::set           (shared with TrackMeta → /queue)
             audio → StreamReader (AsyncRead)
       └─ AsyncAdapterStream (64 KiB buffer)  → symphonia probe/decoder in Songbird
```

* **`IcyParser`** is a three-state machine (`Audio { left }`, `Length`,
  `Meta { left, buf }`). It is correct for **any** chunk boundaries, including a split in the
  middle of a length byte or a metadata block. The test `strips_metadata_at_every_chunk_boundary`
  feeds every chunk size from 1 to 70 bytes.
* **`parse_stream_title`** cuts at the first NUL, decodes UTF-8 and falls back to Latin-1. It
  ends the value at `';StreamUrl=` if present, else at the last `';`, so titles containing
  `'` survive (`Rock 'n' Roll`). It removes control characters, collapses whitespace,
  truncates to 100 chars, and treats an empty title as `None`.
* **`NowPlaying`** is an `RwLock<Option<String>>`. It only writes, and logs at debug level,
  when the title changes.
* **`IcySource`** is not seekable and has no length. The end of the body is the end of the
  track, which `songbird_voice` then treats as a drop (§1.5).
* Errors are stripped of the URL (`without_url`) before they are logged.

### 2.3 Tests

* Unit: `strips_metadata_at_every_chunk_boundary`, `parses_stream_titles`.
* `#[ignore]`, network: `icy_live_station_reports_now_playing` checks that a SomaFM station
  produces a `StreamTitle` within 300 frames and still decodes. Run with
  `cargo test -- --ignored icy_live`. It decodes on a blocking thread because `next_packet`
  blocks while the adapter task refills its buffer.

---

## 3. `pg_library.rs` and sqlx

### 3.1 Schema (`migrations/`)

| Migration | Content |
|---|---|
| `20261011000000_library.sql` | `library_tracks` (one row per (guild, content hash)); `UNIQUE (guild_id, content_hash)`; index on `content_hash` (refcount, dedupe); index on `(guild_id, lower(title))` |
| `20261012000000_title_trigrams.sql` | `CREATE EXTENSION IF NOT EXISTS pg_trgm`; GIN index `lower(title) gin_trgm_ops` (serves both `LIKE` and similarity) |

Discord snowflakes are stored as `BIGINT`. The code casts `u64 as i64` going in and back
coming out. Snowflakes fit in 63 bits.

One S3 object (`<prefix><sha256>.ogg`) can back rows in several guilds. Its lifetime is
reference-counted by the rows that share its `content_hash`.

### 3.2 How sqlx is used

* **Compile-time checked queries**: `query!`, `query_as!(Row, …)` and `query_scalar!` check
  SQL and types against the schema at build time.
  * `Row` mirrors the table, and `TryFrom<Row> for LibraryTrack` converts it into the domain
    type, validating the hash and clamping negative values.
  * The override `AS "name!"` forces non-null where Postgres can't prove it, for example
    `COALESCE(SUM(...))::BIGINT AS "used!"` and `count(*) AS "count!"`.
* **Offline mode**: `.cargo/config.toml` sets `SQLX_OFFLINE=true`, so builds (CI, Docker,
  `make check`) compile from the committed `.sqlx/query-*.json` cache and never contact a
  database. A `DATABASE_URL` in `.env` is not used at compile time.
* **Embedded migrations**: `sqlx::migrate!("./migrations")` compiles the SQL files into the
  binary. The Dockerfile copies `.sqlx/` and `migrations/` into the build stage.
* **Features** (`Cargo.toml`): `runtime-tokio`, `tls-rustls-ring-webpki`, `postgres`,
  `macros`, `migrate`, `chrono`. There are no default features.

**Changing SQL (the workflow):**

```sh
cd discord_bot
# 1. add a new file in migrations/ (never edit an applied one) and/or change a query
make sqlx-prepare   # starts compose Postgres, runs migrations, regenerates .sqlx/
git add .sqlx migrations
make sqlx-check     # what CI runs: fails if .sqlx is stale
```

If you forget `sqlx-prepare`, the build fails with an offline-cache miss for that query.

### 3.3 Connection lifecycle: the bot runs without its database

The library is optional at runtime. A paused Supabase project must not take the bot down.

```text
library.rs:  PgLibrary::connect_lazy(url, DATABASE_MAX_CONNECTIONS=3)   ← no network
             tokio::spawn(repo.clone().supervise())
             tokio::spawn(sweep_periodically(...))

supervise loop:
  available == false → migrate!().run()          (connects + migrates)
                         ok  → log Thai-search support, available = true, backoff = 1 s
                         err → warn, sleep backoff (×2, max 5 min); after 10 min log a
                               "Supabase project may be paused" hint once
  available == true  → every 60 s: SELECT 1
                         every 6 h instead: SELECT count(*) FROM library_tracks
                           (a real table read keeps a free Supabase project from pausing)
                         err → available = false
```

* Every repository method starts with `ensure_available()` (or `begin()`, which calls it).
  While the database is down, calls fail fast with `RepoError::Unavailable` instead of
  waiting for the pool.
* `fail(err)` classifies errors. `Io`, `Tls`, `PoolTimedOut`, `PoolClosed`, `WorkerCrashed`
  and SQLSTATE class `08`/`57` mean the connection was lost, so it flips `available` to false
  and returns `Unavailable`. Anything else is `RepoError::Other`.
* Pool: `acquire_timeout` is 5 s and the pool is small, for **Supavisor session mode**.
  sqlx uses prepared statements, which transaction mode breaks.
* `log_thai_search_support` runs `show_trgm('เพลง')` once per connect. Under
  `LC_CTYPE=C`, Thai produces no trigrams, so Thai search falls back to substring matching
  only.

### 3.4 Consistency between rows and S3 objects: advisory locks

Rows and objects live in two systems with no shared transaction. The invariant is:

> **While the hash lock is held, rows are the truth: if any row has this hash, its object
> exists.**

Every operation that can create or delete an object takes
`pg_advisory_xact_lock(hash.lock_key())`, where `lock_key` is the first 64 bits of the
SHA-256 as `i64`. The object PUT or DELETE happens **while the lock is held**, inside the
transaction. Transaction-level locks release on commit or rollback, so a crash can't leak one.

| Operation | Locks (in order) | Under the lock |
|---|---|---|
| `add` | guild `(GUILD_LOCK_SPACE=1, guild % i32::MAX)`, then hash | already in this guild? → `AlreadyInLibrary`. Another guild has the hash? → reuse its size/duration, no PUT. No row and no audio supplied → `NeedsAudio` (the caller transcodes and retries). Quota check on `SUM(size_bytes)`. PUT if not reused. INSERT. Commit. |
| `remove` | hash | `DELETE … RETURNING` (re-read under the lock; a concurrent delete may have won). If no rows remain for the hash → delete the object. A failed delete rolls back, so the row stays. |
| `sweep` | hash, per listed object | Skips objects younger than the grace period. Count rows; **re-check `last_modified`** (an upload may have re-stored the object since the listing); delete only if 0 rows and still old. |

* **Deadlock freedom**: only `add` takes two locks, and it always takes guild before hash.
  `remove` and `sweep` take only the hash lock.
* **The guild lock** serialises uploads in one guild, so two concurrent uploads can't both
  pass the quota check. The two-int form of the advisory lock is a separate key space from
  the one-bigint hash locks, so they never collide.
* **Orphans**: if the commit fails after a PUT, the object has no row.
  `sweep_periodically` deletes it once it is older than the grace period. The sweep only
  runs while the database is available.

### 3.5 Search (`search`)

One query over a guild's rows. A row is a candidate if any of these hold:

* `lower(title) LIKE %q% ESCAPE '\'`: substring match. It works in every locale, so Thai
  always works. `escape_like` escapes `%`, `_` and `\`.
* `word_similarity(q, lower(title)) >= 0.3` or `similarity(q, lower(title)) >= 0.3`: trigram
  match, for typos and word order. 0.3 is pg_trgm's default threshold.

Results are ranked in this order: exact match, prefix match, substring match, trigram match.
Ties are broken by `greatest(word_similarity, similarity)` descending, then shorter titles,
then newest `id`. Search time is logged under `tatarbot::timing`.

The other reads are `get`, `find_in_guild`, `stored_audio`, `list` (paged, ordered by
`lower(title), id`, plus a total count), `recent` (by `created_at`), and `used_bytes`.

### 3.6 Tests

The tests use `#[sqlx::test(migrations = "./migrations")]`, which creates a fresh, migrated
database for each test from `DATABASE_URL`. They are all `#[ignore]`d and run with
`make test-db` (compose Postgres + RustFS).

They use a `FakeStore` with **gates**: `put_gate` and `delete_gate` pause an operation inside
its critical section. That makes races deterministic. One task enters, the test asserts that
a second task is blocked on the advisory lock (`wait_until_blocked`), then releases the
first.

| Test | Checks |
|---|---|
| `pg_add_stores_once_and_dedupes` | one PUT per hash across guilds |
| `pg_remove_deletes_the_object_with_the_last_row` | refcounted delete |
| `pg_race_delete_vs_upload_of_the_same_hash` | upload after the delete re-stores the object |
| `pg_race_sweep_vs_upload_in_flight` | sweep doesn't delete an object being uploaded |
| `pg_sweep_honours_the_grace_period` | young orphans are kept |
| `pg_race_concurrent_uploads_respect_the_quota` | guild lock serialises the quota check |
| `pg_search_ranks_and_escapes`, `pg_search_is_typo_tolerant_and_ranked`, `pg_search_finds_thai`, `pg_search_c_locale_falls_back_to_substrings` | search behaviour, including a temporary `LC_CTYPE=C` database |
| `pg_unavailable_fails_fast` | `Unavailable` without waiting for the pool |

---

## 4. Where to change things

| To… | Edit |
|---|---|
| change radio backoff or give-up limits | `RECONNECT_BASE`, `MAX_RECONNECTS`, `STABLE_AFTER` in `songbird_voice.rs` (and the `Reconnects` tests) |
| add a new kind of track source | `TrackSource` (domain), `Inner::songbird_track`, `TrackMeta::kind` |
| change how long presigned URLs live | `PRESIGN_TTL`; keep it longer than the maximum track length |
| change the title length or parsing of ICY titles | `MAX_TITLE_CHARS`, `parse_stream_title` in `icy.rs` |
| change the schema | a **new** file in `migrations/`, then `make sqlx-prepare` and commit `.sqlx/` |
| tune fuzzy search | `SIMILARITY_THRESHOLD` and the `ORDER BY` in `PgLibrary::search` |
| change health-check or keep-alive timing | `HEALTH_INTERVAL`, `KEEP_ALIVE_INTERVAL`, `MAX_BACKOFF` in `pg_library.rs` |
