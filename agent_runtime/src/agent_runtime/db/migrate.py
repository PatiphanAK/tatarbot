"""Applies the SQL migrations in `migrations/` to DATABASE_URL.

Run with: python -m agent_runtime.db.migrate

Files are named `<version>_<description>.sql` and applied in version order, each in its
own transaction, like the bot's sqlx migrations. Applied versions are recorded with a
checksum in `_agent_runtime_migrations` (separate from sqlx's `_sqlx_migrations`, so both
services can share one database). Editing an applied file is an error: add a new one.
An advisory lock serializes concurrent runs.
"""

import hashlib
import logging
import os
import re
import sys
from dataclasses import dataclass
from importlib import resources

import psycopg

log = logging.getLogger("agent_runtime.migrate")

# Arbitrary, fixed: pg_advisory_lock key shared by every runner of these migrations.
_LOCK_KEY = 0x7A7A_0001
_NAME = re.compile(r"^(\d+)_([a-z0-9_]+)\.sql$")


class MigrationError(Exception):
    pass


@dataclass(frozen=True)
class Migration:
    version: int
    description: str
    sql: str

    @property
    def checksum(self) -> bytes:
        return hashlib.sha384(self.sql.encode()).digest()


def load_migrations() -> list[Migration]:
    """The packaged migrations, oldest first."""
    migrations = []
    for entry in resources.files("agent_runtime.db").joinpath("migrations").iterdir():
        if not entry.name.endswith(".sql"):
            continue
        match = _NAME.match(entry.name)
        if match is None:
            raise MigrationError(f"bad migration file name: {entry.name}")
        migrations.append(
            Migration(int(match[1]), match[2].replace("_", " "), entry.read_text("utf-8"))
        )
    migrations.sort(key=lambda m: m.version)
    versions = [m.version for m in migrations]
    if len(set(versions)) != len(versions):
        raise MigrationError("duplicate migration version")
    return migrations


def migrate(conninfo: str, migrations: list[Migration] | None = None) -> list[Migration]:
    """Applies pending migrations; returns the ones applied."""
    migrations = load_migrations() if migrations is None else migrations
    with psycopg.connect(conninfo, autocommit=True) as conn:
        conn.execute("SELECT pg_advisory_lock(%s)", (_LOCK_KEY,))
        try:
            conn.execute(
                """CREATE TABLE IF NOT EXISTS _agent_runtime_migrations (
                       version     BIGINT      PRIMARY KEY,
                       description TEXT        NOT NULL,
                       checksum    BYTEA       NOT NULL,
                       applied_at  TIMESTAMPTZ NOT NULL DEFAULT now()
                   )"""
            )
            applied = dict(conn.execute("SELECT version, checksum FROM _agent_runtime_migrations"))
            _check_applied(applied, migrations)
            pending = [m for m in migrations if m.version not in applied]
            for m in pending:
                log.info("applying migration %d %s", m.version, m.description)
                with conn.transaction():
                    conn.execute(m.sql)
                    conn.execute(
                        "INSERT INTO _agent_runtime_migrations (version, description, checksum)"
                        " VALUES (%s, %s, %s)",
                        (m.version, m.description, m.checksum),
                    )
            return pending
        finally:
            conn.execute("SELECT pg_advisory_unlock(%s)", (_LOCK_KEY,))


def _check_applied(applied: dict[int, bytes], migrations: list[Migration]) -> None:
    known = {m.version: m for m in migrations}
    for version, checksum in sorted(applied.items()):
        m = known.get(version)
        if m is None:
            raise MigrationError(f"migration {version} was applied but is missing here")
        if bytes(checksum) != m.checksum:
            raise MigrationError(f"migration {version} was modified after it was applied")


def main() -> int:
    logging.basicConfig(level=logging.INFO, format="%(levelname)s:     %(name)s: %(message)s")
    conninfo = os.environ.get("DATABASE_URL", "").strip()
    if not conninfo:
        log.error("missing required environment variable DATABASE_URL")
        return 2
    try:
        applied = migrate(conninfo)
    except (MigrationError, psycopg.Error) as e:
        log.error("migration failed: %s", e)
        return 1
    log.info("%d migration(s) applied", len(applied))
    return 0


if __name__ == "__main__":
    sys.exit(main())
