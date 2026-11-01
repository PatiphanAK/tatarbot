# db

The runtime's Postgres schema (`rag`) and the runner that applies it. The schema is used
only with `RAG_STORE=postgres`; how the data flows through it is in
[docs/architecture/database.md](../../../../docs/architecture/database.md).

| File | Purpose |
|---|---|
| `migrate.py` | Runner: `python -m agent_runtime.db.migrate` (also in the image) |
| `migrations/20261031000000_rag.sql` | `vector` extension, `rag.documents`, `rag.chunks` (`vector(1024)`, HNSW cosine index) |
| `migrations/20261101000000_rag_ingestion.sql` | `pg_trgm`; storage and status columns on documents; `chunks.heading` and trigram index; `rag.ingestion_jobs` |

## Usage

```sh
make -C agent_runtime db-up     # local Postgres + pgvector (../compose.yaml)
make -C agent_runtime migrate   # apply pending migrations to DATABASE_URL from .env
```

The app never migrates on startup. Apply migrations before deploying code that needs them.

## How the runner works

* Files are `<version>_<description>.sql` (version = UTC timestamp, description lowercase
  with underscores), packaged with the code and applied in version order.
* Each file runs in its own transaction, together with its row in
  `_agent_runtime_migrations` (version, description, SHA-384 checksum). A failing file
  leaves nothing behind.
* Before applying, every recorded version must exist locally with the same checksum:
  an edited or deleted applied file is an error.
* A session-level advisory lock serializes concurrent runners. On Supabase use the direct
  or session-pooler URL (port 5432), not the transaction pooler.
* The history table is separate from the bot's `_sqlx_migrations`, so both services can
  migrate the same database independently.

## Adding a migration

1. New file `migrations/<UTC yyyymmddHHMMSS>_<what>.sql`. Never edit an applied one.
2. Keep it additive where possible, so the running version keeps working until the new
   code is deployed (new columns nullable or with defaults; constraints that existing rows
   satisfy).
3. Changing the embedding dimension means altering `rag.chunks.embedding` and its index,
   updating `RAG_SCHEMA_DIMENSIONS` in `config.py`, and re-embedding every document.
4. Test it: `make -C agent_runtime test-db`. `tests/test_migrate.py` applies all migrations
   to a fresh database. Add a test there when a migration must preserve existing rows.

## Constraints

* Needs the `vector` and `pg_trgm` extensions (trusted on Supabase; included in the
  `pgvector/pgvector` image used locally and in CI).
* Thai keyword search needs a database whose `LC_CTYPE` classifies Thai letters as
  alphanumeric (`en_US.UTF-8`); `test_trigram_matching_handles_thai` checks it.
