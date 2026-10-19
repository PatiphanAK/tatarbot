# discord_bot docs

Code-level design notes for maintainers. For what the bot does and how to run it, see
[`../README.md`](../README.md) and the [top-level README](../../README.md).

| Document | Covers |
|---|---|
| [outbound-adapters.md](outbound-adapters.md) | `songbird_voice` (playback, radio reconnect), `icy` (live stream titles), `pg_library` + sqlx (schema, offline cache, availability, advisory locks, search) |
