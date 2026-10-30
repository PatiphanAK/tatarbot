"""Migration runner. Tests marked `db` need Postgres with pgvector at TEST_DATABASE_URL
(`make test-db`); each gets a fresh database, dropped afterwards. Skipped without it."""

import os
import uuid
from dataclasses import replace

import psycopg
import pytest
from psycopg.conninfo import make_conninfo

from agent_runtime.db.migrate import Migration, MigrationError, load_migrations, migrate

TEST_DATABASE_URL = os.environ.get("TEST_DATABASE_URL", "").strip()
db = pytest.mark.skipif(not TEST_DATABASE_URL, reason="TEST_DATABASE_URL not set")


def test_packaged_migrations_are_ordered_and_named():
    migrations = load_migrations()
    assert migrations, "no migrations packaged"
    versions = [m.version for m in migrations]
    assert versions == sorted(set(versions))
    assert migrations[0].description == "rag"


@pytest.fixture
def database():
    name = f"test_migrate_{uuid.uuid4().hex[:12]}"
    with psycopg.connect(TEST_DATABASE_URL, autocommit=True) as admin:
        admin.execute(f'CREATE DATABASE "{name}"')
        try:
            yield make_conninfo(TEST_DATABASE_URL, dbname=name)
        finally:
            admin.execute(f'DROP DATABASE "{name}" WITH (FORCE)')


@db
def test_applies_once(database):
    assert [m.version for m in migrate(database)] == [m.version for m in load_migrations()]
    assert migrate(database) == []


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
    broken = Migration(99990101000000, "broken", "CREATE TABLE half (id INT); SELECT 1/0;")
    with pytest.raises(psycopg.errors.DivisionByZero):
        migrate(database, [broken])
    with psycopg.connect(database) as conn:
        assert conn.execute("SELECT to_regclass('half')").fetchone()[0] is None
        assert conn.execute("SELECT count(*) FROM _agent_runtime_migrations").fetchone()[0] == 0
