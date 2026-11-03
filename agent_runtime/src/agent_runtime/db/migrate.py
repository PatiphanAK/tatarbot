"""Applies the SQL migrations in `migrations/<group>/` to Postgres.

Run with: python -m agent_runtime.db.migrate [rag] [runtime]   (no argument: both)

Two groups share one database and one bookkeeping table but not each other's tables:
- `rag`: the knowledge base (schema `rag`, pgvector, pg_trgm), applied to DATABASE_URL;
- `runtime`: runtime state such as conversation history (schema `runtime`, plain relational
  tables), applied to RUNTIME_DATABASE_URL, or DATABASE_URL when that is empty.
Either group can be applied alone and in any order; versions are unique across groups.

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
from collections.abc import Mapping
from dataclasses import dataclass
from importlib import resources

import psycopg

log = logging.getLogger("agent_runtime.migrate")

# Arbitrary, fixed: pg_advisory_lock key shared by every runner of these migrations.
_LOCK_KEY = 0x7A7A_0001
_NAME = re.compile(r"^(\d+)_([a-z0-9_]+)\.sql$")
GROUPS = ("rag", "runtime")


class MigrationError(Exception):
    pass


@dataclass(frozen=True)
class Migration:
    group: str
    version: int
    description: str
    sql: str

    @property
    def checksum(self) -> bytes:
        return hashlib.sha384(self.sql.encode()).digest()


def load_migrations() -> list[Migration]:
    """The packaged migrations of every group, oldest first."""
    root = resources.files("agent_runtime.db").joinpath("migrations")
    migrations = []
    for entry in root.iterdir():
        if entry.name.endswith(".sql"):
            raise MigrationError(f"migration outside a group directory: {entry.name}")
    for group in GROUPS:
        for entry in root.joinpath(group).iterdir():
            if not entry.name.endswith(".sql"):
                continue
            match = _NAME.match(entry.name)
            if match is None:
                raise MigrationError(f"bad migration file name: {group}/{entry.name}")
            migrations.append(
                Migration(
                    group, int(match[1]), match[2].replace("_", " "), entry.read_text("utf-8")
                )
            )
    migrations.sort(key=lambda m: m.version)
    versions = [m.version for m in migrations]
    if len(set(versions)) != len(versions):
        raise MigrationError("duplicate migration version")
    return migrations


def migrate(
    conninfo: str,
    migrations: list[Migration] | None = None,
    groups: tuple[str, ...] = GROUPS,
) -> list[Migration]:
    """Applies the pending migrations of `groups`; returns the ones applied.

    Applied versions are checked against every group, so a database that already has the
    other group's migrations is fine."""
    migrations = load_migrations() if migrations is None else migrations
    with psycopg.connect(conninfo, autocommit=True) as conn:
        conn.execute("SELECT pg_advisory_lock(%s)", (_LOCK_KEY,))
        try:
            conn.execute(
                """CREATE TABLE IF NOT EXISTS public._agent_runtime_migrations (
                       version     BIGINT      PRIMARY KEY,
                       description TEXT        NOT NULL,
                       checksum    BYTEA       NOT NULL,
                       applied_at  TIMESTAMPTZ NOT NULL DEFAULT now()
                   )"""
            )
            applied = dict(
                conn.execute("SELECT version, checksum FROM public._agent_runtime_migrations")
            )
            _check_applied(applied, migrations)
            pending = [m for m in migrations if m.group in groups and m.version not in applied]
            for m in pending:
                log.info("applying migration %s/%d %s", m.group, m.version, m.description)
                with conn.transaction():
                    conn.execute(m.sql)
                    conn.execute(
                        "INSERT INTO public._agent_runtime_migrations"
                        " (version, description, checksum) VALUES (%s, %s, %s)",
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


def conninfo_for(group: str, env: Mapping[str, str] = os.environ) -> str | None:
    """`runtime` uses RUNTIME_DATABASE_URL when set; everything else uses DATABASE_URL."""
    database_url = env.get("DATABASE_URL", "").strip()
    if group == "runtime":
        return env.get("RUNTIME_DATABASE_URL", "").strip() or database_url or None
    return database_url or None


def main(argv: list[str] | None = None) -> int:
    logging.basicConfig(level=logging.INFO, format="%(levelname)s:     %(name)s: %(message)s")
    groups = tuple(sys.argv[1:] if argv is None else argv) or GROUPS
    unknown = [g for g in groups if g not in GROUPS]
    if unknown:
        log.error("unknown migration group(s) %s; expected %s", unknown, ", ".join(GROUPS))
        return 2
    targets: dict[str, list[str]] = {}
    for group in GROUPS:
        if group not in groups:
            continue
        conninfo = conninfo_for(group)
        if conninfo is None:
            log.error("missing required environment variable DATABASE_URL (for %s)", group)
            return 2
        targets.setdefault(conninfo, []).append(group)
    total = 0
    try:
        for conninfo, target_groups in targets.items():
            total += len(migrate(conninfo, groups=tuple(target_groups)))
    except (MigrationError, psycopg.Error) as e:
        log.error("migration failed: %s", e)
        return 1
    log.info("%d migration(s) applied (%s)", total, ", ".join(groups))
    return 0


if __name__ == "__main__":
    sys.exit(main())
