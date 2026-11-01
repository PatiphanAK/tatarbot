"""PostgresVectorStore against a real Postgres + pgvector (`make test-db`)."""

import psycopg
import pytest
from conftest import db

from agent_runtime.adapters.outbound.postgres_rag import PostgresVectorStore, create_pool
from agent_runtime.db.migrate import migrate
from agent_runtime.domain import Chunk, EmbeddingError

pytestmark = [pytest.mark.anyio, db]

DIM = 1024
MODEL = "test:onehot:1024"


def axis(i: int, weight: float = 1.0, other: int | None = None) -> list[float]:
    """A unit-ish vector along axis i, optionally tilted toward another axis."""
    v = [0.0] * DIM
    v[i] = weight
    if other is not None:
        v[other] = 1.0
    return v


def chunk(doc: str, text: str, index: int = 0, heading: str = "") -> Chunk:
    return Chunk(doc, doc.removesuffix(".md").title(), heading, text, index)


@pytest.fixture
async def pool(database):
    migrate(database)
    pool = create_pool(database, 3)
    await pool.open()
    yield pool
    await pool.close()


def add_upload(database, namespace, source, chunks, model=MODEL):
    """An uploaded, ready document written straight to the tables."""
    with psycopg.connect(database) as conn:
        doc = conn.execute(
            "INSERT INTO rag.documents (namespace, source, title, content_hash, storage_provider,"
            " storage_bucket, object_key) VALUES (%s, %s, %s, %s, 's3', 'bucket', %s) RETURNING id",
            (namespace, source, source, "c" * 64, f"rag/{namespace}/{source}"),
        ).fetchone()[0]
        for i, (text, vector) in enumerate(chunks):
            conn.execute(
                "INSERT INTO rag.chunks (document_id, ordinal, content, embedding,"
                " embedding_model) VALUES (%s, %s, %s, %s::vector, %s)",
                (doc, i, text, str(vector), model),
            )
    return doc


async def test_directory_index_persists_and_is_replaced(pool):
    store = PostgresVectorStore(pool, MODEL, DIM)
    await store.replace(
        [chunk("a.md", "alpha text", heading="Intro"), chunk("b.md", "beta text")],
        [axis(0), axis(1)],
    )
    # A new store (another process, a restart) sees the same index.
    again = PostgresVectorStore(pool, MODEL, DIM)
    assert await again.count() == 2
    hits = await again.search(axis(0), 1)
    assert hits[0].chunk == chunk("a.md", "alpha text", heading="Intro")
    assert hits[0].score == pytest.approx(1.0)

    # Re-indexing drops files that are gone and replaces changed ones.
    await again.replace([chunk("a.md", "alpha v2")], [axis(2)])
    assert await again.count() == 1
    assert (await again.search(axis(2), 5))[0].chunk.text == "alpha v2"


async def test_directory_index_leaves_uploads_alone(pool, database):
    add_upload(database, "global", "faq.md", [("uploaded faq", axis(3))])
    store = PostgresVectorStore(pool, MODEL, DIM)
    # A directory file with an upload's source is skipped, and the upload survives.
    await store.replace([chunk("faq.md", "directory faq"), chunk("a.md", "a")], [axis(4), axis(5)])
    await store.replace([], [])
    hits = await store.search(axis(3), 5)
    assert [h.chunk.text for h in hits] == ["uploaded faq"]


async def test_namespaces_are_isolated(pool, database):
    add_upload(database, "global", "rules.md", [("global rules", axis(0))])
    add_upload(database, "discord:1", "one.md", [("server one secret", axis(0, 0.9, 1))])
    add_upload(database, "discord:2", "two.md", [("server two secret", axis(0, 0.9, 2))])
    store = PostgresVectorStore(pool, MODEL, DIM)

    texts = {
        h.chunk.text for h in await store.search(axis(0), 10, namespaces=["global", "discord:1"])
    }
    assert texts == {"global rules", "server one secret"}
    # No namespaces means the global one only, never everything.
    assert {h.chunk.text for h in await store.search(axis(0), 10)} == {"global rules"}
    # Keyword matches respect namespaces too.
    hits = await store.search(axis(9), 10, text="server two secret", namespaces=["discord:1"])
    assert all("two" not in h.chunk.text for h in hits)


async def test_other_embedding_models_are_not_searched(pool, database):
    add_upload(database, "global", "old.md", [("old model chunk", axis(0))], model="other:model")
    store = PostgresVectorStore(pool, MODEL, DIM)
    assert await store.search(axis(0), 5, text="old model chunk") == []
    assert await store.count() == 0


async def test_keyword_match_is_fused_with_semantic_ranking(pool, database):
    add_upload(
        database,
        "global",
        "guide.md",
        [
            ("Use /upload with an audio file up to 25 MB.", axis(5)),  # far from the query
            ("General chat etiquette.", axis(0, 0.9, 1)),
            ("Radio stations and schedules.", axis(0, 0.8, 2)),
        ],
    )
    store = PostgresVectorStore(pool, MODEL, DIM)
    hits = await store.search(axis(0), 3, text="how do I /upload a file")
    by_text = {h.chunk.text: h for h in hits}
    upload = by_text["Use /upload with an audio file up to 25 MB."]
    # Semantically unrelated, but found by keyword and carrying its trigram score.
    assert upload.keyword_score is not None and upload.keyword_score > 0.25
    assert upload.score < 0.1
    assert by_text["General chat etiquette."].keyword_score is None
    # Without the keyword text it is not among the semantic results' top 2.
    assert "Use /upload" not in " ".join(h.chunk.text for h in await store.search(axis(0), 2))


async def test_both_rankings_beat_one(pool, database):
    add_upload(
        database,
        "global",
        "g.md",
        [("lofi radio station", axis(0, 0.9, 1)), ("unrelated words", axis(0))],
    )
    store = PostgresVectorStore(pool, MODEL, DIM)
    hits = await store.search(axis(0), 2, text="lofi radio")
    # Second semantically, but also the only keyword match: RRF puts it first.
    assert hits[0].chunk.text == "lofi radio station"


async def test_thai_keyword_search(pool, database):
    add_upload(
        database,
        "global",
        "th.md",
        [
            ("ไฟล์เพลงที่อัปโหลดต้องมีขนาดไม่เกิน 25 MB", axis(7)),
            ("ฟังวิทยุด้วยคำสั่ง /radio", axis(8)),
        ],
    )
    store = PostgresVectorStore(pool, MODEL, DIM)
    hits = await store.search(axis(0), 2, text="อัปโหลดเพลงได้ใหญ่สุดเท่าไหร่")
    keyword = [h.chunk.text for h in hits if h.keyword_score is not None]
    assert keyword == ["ไฟล์เพลงที่อัปโหลดต้องมีขนาดไม่เกิน 25 MB"]


async def test_wrong_dimensions_are_rejected(pool):
    store = PostgresVectorStore(pool, MODEL, DIM)
    with pytest.raises(EmbeddingError, match="3 dimensions"):
        await store.replace([chunk("a.md", "x")], [[1.0, 0.0, 0.0]])
    with pytest.raises(EmbeddingError, match="3 dimensions"):
        await store.search([1.0, 0.0, 0.0], 1)


async def test_schema_mismatch_is_detected(pool):
    """A runtime configured for another size refuses to use the 1024-d schema."""
    store = PostgresVectorStore(pool, MODEL, 512)
    with pytest.raises(EmbeddingError, match="stores 1024-dimensional vectors"):
        await store.search([1.0] * 512, 1)
