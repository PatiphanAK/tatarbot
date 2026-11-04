"""Migration runner. Tests marked `db` need Postgres with pgvector at TEST_DATABASE_URL
(`make test-db`); each gets a fresh database, dropped afterwards. Skipped without it."""

from dataclasses import replace

import psycopg
import pytest
from conftest import db

from agent_runtime.db.migrate import (
    GROUPS,
    Migration,
    MigrationError,
    conninfo_for,
    load_migrations,
    main,
    migrate,
)


def test_packaged_migrations_are_ordered_and_named():
    migrations = load_migrations()
    assert migrations, "no migrations packaged"
    versions = [m.version for m in migrations]
    assert versions == sorted(set(versions))
    assert migrations[0].description == "rag"
    assert {m.group for m in migrations} == set(GROUPS)


def test_groups_hold_their_own_schema_only():
    """RAG migrations touch only `rag`; runtime migrations only `runtime`, with no vectors."""
    for m in load_migrations():
        sql = "\n".join(line.split("--")[0] for line in m.sql.lower().splitlines())
        if m.group == "rag":
            assert "runtime." not in sql
        else:
            assert "rag." not in sql
            assert "vector" not in sql


def test_runtime_url_falls_back_to_database_url():
    env = {"DATABASE_URL": "postgres://main"}
    assert conninfo_for("rag", env) == "postgres://main"
    assert conninfo_for("runtime", env) == "postgres://main"
    env["RUNTIME_DATABASE_URL"] = "postgres://runtime"
    assert conninfo_for("runtime", env) == "postgres://runtime"
    assert conninfo_for("rag", env) == "postgres://main"
    assert conninfo_for("runtime", {"RUNTIME_DATABASE_URL": " "}) is None


def test_unknown_group_is_rejected():
    assert main(["chat"]) == 2


@db
def test_applies_once(database):
    assert [m.version for m in migrate(database)] == [m.version for m in load_migrations()]
    assert migrate(database) == []


@db
@pytest.mark.parametrize("first", GROUPS)
def test_groups_apply_independently_in_any_order(database, first):
    second = next(g for g in GROUPS if g != first)
    by_group = {g: [m.version for m in load_migrations() if m.group == g] for g in GROUPS}
    assert [m.version for m in migrate(database, groups=(first,))] == by_group[first]
    with psycopg.connect(database) as conn:
        schemas = {r[0] for r in conn.execute("SELECT nspname FROM pg_namespace")}
    assert first in schemas and second not in schemas
    assert [m.version for m in migrate(database, groups=(second,))] == by_group[second]
    assert migrate(database) == []


@db
def test_runtime_schema_is_relational(database):
    migrate(database, groups=("runtime",))
    with psycopg.connect(database) as conn:
        conn.execute(
            "INSERT INTO runtime.conversations (conversation_id, messages) VALUES ('c', '[]')"
        )
        with pytest.raises(psycopg.errors.CheckViolation):
            conn.execute(
                "INSERT INTO runtime.conversations (conversation_id, messages) VALUES ('d', '{}')"
            )
    with psycopg.connect(database) as conn:
        # Neither pgvector nor pg_trgm is needed for the runtime group.
        assert (
            conn.execute(
                "SELECT count(*) FROM pg_extension WHERE extname IN ('vector', 'pg_trgm')"
            ).fetchone()[0]
            == 0
        )


@db
def test_main_uses_one_database_for_both_groups(database, monkeypatch):
    monkeypatch.setenv("DATABASE_URL", database)
    monkeypatch.delenv("RUNTIME_DATABASE_URL", raising=False)
    assert main([]) == 0
    with psycopg.connect(database) as conn:
        assert conn.execute("SELECT to_regclass('rag.chunks')").fetchone()[0] is not None
        assert conn.execute("SELECT to_regclass('runtime.conversations')").fetchone()[0] is not None


@db
def test_rag_schema_supports_similarity_search(database):
    migrate(database)
    with psycopg.connect(database) as conn:
        doc = conn.execute(
            "INSERT INTO rag.documents (namespace, source, content_hash)"
            " VALUES ('global', 'doc.md', %s) RETURNING id",
            ("a" * 64,),
        ).fetchone()[0]
        near, far = [1.0] + [0.0] * 1023, [0.0] * 1023 + [1.0]
        for ordinal, vec in enumerate([far, near]):
            conn.execute(
                "INSERT INTO rag.chunks (document_id, ordinal, content, embedding, embedding_model)"
                " VALUES (%s, %s, %s, %s::vector, 'bge-m3')",
                (doc, ordinal, f"chunk {ordinal}", str(vec)),
            )
        best = conn.execute(
            "SELECT c.content FROM rag.chunks c JOIN rag.documents d ON d.id = c.document_id"
            " WHERE d.namespace = 'global' ORDER BY c.embedding <=> %s::vector LIMIT 1",
            (str(near),),
        ).fetchone()[0]
        assert best == "chunk 1"
        conn.execute("DELETE FROM rag.documents WHERE id = %s", (doc,))
        assert conn.execute("SELECT count(*) FROM rag.chunks").fetchone()[0] == 0


@db
def test_rejects_wrong_embedding_dimension(database):
    migrate(database)
    with psycopg.connect(database) as conn:
        doc = conn.execute(
            "INSERT INTO rag.documents (namespace, source, content_hash)"
            " VALUES ('global', 'doc.md', %s) RETURNING id",
            ("a" * 64,),
        ).fetchone()[0]
        with pytest.raises(psycopg.errors.DataException):
            conn.execute(
                "INSERT INTO rag.chunks (document_id, ordinal, content, embedding, embedding_model)"
                " VALUES (%s, 0, 'x', '[1,2,3]'::vector, 'bge-m3')",
                (doc,),
            )


@db
def test_modified_migration_is_an_error(database):
    migrate(database)
    first, *rest = load_migrations()
    edited = replace(first, sql=first.sql + "\n-- edited\n")
    with pytest.raises(MigrationError, match="modified"):
        migrate(database, [edited, *rest])


@db
def test_missing_migration_is_an_error(database):
    migrate(database)
    with pytest.raises(MigrationError, match="missing"):
        migrate(database, [])


@db
def test_failed_migration_is_rolled_back(database):
    broken = Migration(
        "runtime", 99990101000000, "broken", "CREATE TABLE half (id INT); SELECT 1/0;"
    )
    with pytest.raises(psycopg.errors.DivisionByZero):
        migrate(database, [broken])
    with psycopg.connect(database) as conn:
        assert conn.execute("SELECT to_regclass('half')").fetchone()[0] is None
        assert (
            conn.execute("SELECT count(*) FROM public._agent_runtime_migrations").fetchone()[0] == 0
        )


@db
def test_ingestion_migration_keeps_existing_rows(database):
    first, *rest = load_migrations()
    assert first.group == "rag"
    migrate(database, [first])
    with psycopg.connect(database) as conn:
        doc = conn.execute(
            "INSERT INTO rag.documents (namespace, source, content_hash)"
            " VALUES ('global', 'old.md', %s) RETURNING id",
            ("b" * 64,),
        ).fetchone()[0]
        conn.execute(
            "INSERT INTO rag.chunks (document_id, ordinal, content, embedding, embedding_model)"
            " VALUES (%s, 0, 'old chunk', %s::vector, 'bge-m3')",
            (doc, str([1.0] * 1024)),
        )
    migrate(database)
    with psycopg.connect(database) as conn:
        status, provider = conn.execute(
            "SELECT status, storage_provider FROM rag.documents WHERE id = %s", (doc,)
        ).fetchone()
        assert (status, provider) == ("ready", None)
        assert conn.execute("SELECT heading FROM rag.chunks").fetchone()[0] == ""


@db
def test_ingestion_schema_constraints(database):
    migrate(database)
    with psycopg.connect(database, autocommit=True) as conn:
        # A pending upload has no hash yet...
        doc = conn.execute(
            "INSERT INTO rag.documents (namespace, source, storage_provider, storage_bucket,"
            " object_key, status) VALUES ('global', 'a.md', 's3', 'bucket', 'rag/a.md',"
            " 'pending_upload') RETURNING id"
        ).fetchone()[0]
        # ...but a ready document must have one.
        with pytest.raises(psycopg.errors.CheckViolation):
            conn.execute("UPDATE rag.documents SET status = 'ready' WHERE id = %s", (doc,))
        # An S3 document needs its location.
        with pytest.raises(psycopg.errors.CheckViolation):
            conn.execute(
                "INSERT INTO rag.documents (namespace, source, storage_provider, status)"
                " VALUES ('global', 'b.md', 's3', 'pending_upload')"
            )
        with pytest.raises(psycopg.errors.CheckViolation):
            conn.execute("UPDATE rag.documents SET status = 'uploaded' WHERE id = %s", (doc,))
        # One job per object and pipeline configuration.
        job = (
            "INSERT INTO rag.ingestion_jobs (document_id, object_key, parser_version,"
            " chunker_version, embedding_model) VALUES (%s, 'rag/a.md', 'p', 'c', 'm')"
        )
        conn.execute(job, (doc,))
        with pytest.raises(psycopg.errors.UniqueViolation):
            conn.execute(job, (doc,))
        conn.execute("DELETE FROM rag.documents WHERE id = %s", (doc,))
        assert conn.execute("SELECT count(*) FROM rag.ingestion_jobs").fetchone()[0] == 0


@db
def test_trigram_matching_handles_thai(database):
    """Keyword search relies on the database classifying Thai letters as alphanumeric."""
    migrate(database)
    with psycopg.connect(database) as conn:
        thai = conn.execute(
            "SELECT word_similarity('อัปโหลดเพลง', 'คุณสามารถอัปโหลดเพลงได้ไม่เกิน 25 MB')"
        ).fetchone()[0]
        assert thai > 0.5
