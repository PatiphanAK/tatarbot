"""Postgres + pgvector adapters over the `rag` schema (migrations in `agent_runtime.db`).

`PostgresVectorStore` is the `VectorStore`: RAG_DOCS_DIR's documents live in the global
namespace, uploaded ones in theirs, and a search fuses two rankings with Reciprocal Rank
Fusion: cosine distance over the HNSW index, and trigram word similarity (pg_trgm, which
works for Thai, unlike full-text search). Only chunks embedded by the configured model
are searched, so vectors of different models are never compared.

`PostgresDocumentRepository` keeps uploaded documents and their ingestion jobs. Rows are
locked document first, then job, everywhere, so concurrent workers and deletes cannot
deadlock each other.

Vectors travel as pgvector text literals (`'[0.1,0.2,...]'::vector`): no extra driver
dependency. Statements are never prepared and settings are transaction-local, so a
transaction pooler (Supabase :6543, pgbouncer) should also work; that is untested.
"""

import hashlib
import logging
from collections.abc import Sequence

from psycopg import AsyncConnection
from psycopg.rows import dict_row
from psycopg_pool import AsyncConnectionPool

from agent_runtime.application.ports import DocumentRepository, VectorStore
from agent_runtime.domain import (
    Chunk,
    DocumentStatus,
    EmbeddingError,
    InvalidInputError,
    RetrievedChunk,
    StoredDocument,
)
from agent_runtime.domain.ingestion import GLOBAL_NAMESPACE, Claim, IngestionJob, JobKey

log = logging.getLogger(__name__)

# Reciprocal Rank Fusion constant (Cormack et al.): a chunk's fused score is the sum of
# 1 / (k + rank) over the rankings it appears in. 60 is the customary value.
RRF_K = 60
# Each ranking contributes this many candidates per requested chunk (at least 20).
_CANDIDATES_PER_RESULT = 5
# Keyword matching looks at the start of long messages only.
_MAX_KEYWORD_QUERY_CHARS = 500
# Serializes RAG_DOCS_DIR re-indexing across runtime replicas.
_DIRECTORY_LOCK_KEY = 0x7A7A_0002


def create_pool(conninfo: str, max_size: int) -> AsyncConnectionPool:
    """Opened by the caller (`await pool.open()`), closed with `await pool.close()`."""
    return AsyncConnectionPool(
        conninfo,
        min_size=1,
        max_size=max_size,
        open=False,
        # Waiting for a connection; retrieval has its own, shorter, turn budget.
        timeout=10,
        kwargs={"prepare_threshold": None},
        name="rag",
    )


def vector_literal(vector: Sequence[float]) -> str:
    return "[" + ",".join(repr(float(x)) for x in vector) + "]"


async def schema_dimensions(conn: AsyncConnection) -> int:
    """The size of rag.chunks.embedding (pgvector stores it as the type modifier)."""
    row = await (
        await conn.execute(
            "SELECT atttypmod FROM pg_attribute"
            " WHERE attrelid = 'rag.chunks'::regclass AND attname = 'embedding'"
        )
    ).fetchone()
    if row is None:
        raise EmbeddingError("rag.chunks.embedding is missing: run the migrations")
    return row[0]


class _DimensionCheck:
    """Compares vectors with the live schema once it has been read."""

    def __init__(self, expected: int):
        self._expected = expected
        self._verified = False

    async def verify(self, conn: AsyncConnection) -> None:
        if self._verified:
            return
        actual = await schema_dimensions(conn)
        if actual != self._expected:
            raise EmbeddingError(
                f"rag.chunks stores {actual}-dimensional vectors, the runtime is configured "
                f"for {self._expected}: migrate the schema and re-embed, or fix the config"
            )
        self._verified = True

    def check(self, vectors: Sequence[Sequence[float]]) -> None:
        for v in vectors:
            if len(v) != self._expected:
                raise EmbeddingError(
                    f"embedding has {len(v)} dimensions, the RAG schema stores "
                    f"{self._expected}; the embedding model or EMBEDDING_DIMENSIONS is wrong"
                )


class PostgresVectorStore(VectorStore):
    shared = True

    def __init__(
        self,
        pool: AsyncConnectionPool,
        embedding_model: str,
        dimensions: int,
        *,
        keyword_min_score: float = 0.25,
        rrf_k: int = RRF_K,
    ):
        self._pool = pool
        self._model = embedding_model
        self._dimensions = _DimensionCheck(dimensions)
        self._keyword_min_score = keyword_min_score
        self._rrf_k = rrf_k

    async def replace(self, chunks: Sequence[Chunk], vectors: Sequence[list[float]]) -> None:
        """Make the global namespace's directory documents exactly `chunks`' documents, in
        one transaction. Uploaded documents are not touched; a directory file whose path
        is already an uploaded document's source in the global namespace is skipped."""
        if len(chunks) != len(vectors):
            raise ValueError("one vector per chunk")
        self._dimensions.check(vectors)
        by_document: dict[str, list[tuple[Chunk, list[float]]]] = {}
        for chunk, vector in zip(chunks, vectors, strict=True):
            by_document.setdefault(chunk.document_id, []).append((chunk, vector))

        async with self._pool.connection() as conn, conn.transaction():
            await self._dimensions.verify(conn)
            await conn.execute("SELECT pg_advisory_xact_lock(%s)", (_DIRECTORY_LOCK_KEY,))
            await conn.execute(
                "DELETE FROM rag.documents WHERE namespace = %s"
                " AND storage_provider = 'directory' AND NOT (source = ANY(%s))",
                (GLOBAL_NAMESPACE, list(by_document)),
            )
            for source, items in by_document.items():
                content_hash = hashlib.sha256(
                    "\n".join(c.key for c, _ in items).encode()
                ).hexdigest()
                row = await (
                    await conn.execute(
                        """INSERT INTO rag.documents
                               (namespace, source, title, content_hash, storage_provider,
                                status, processed_at)
                           VALUES (%s, %s, %s, %s, 'directory', 'ready', now())
                           ON CONFLICT (namespace, source) DO UPDATE
                           SET title = EXCLUDED.title, content_hash = EXCLUDED.content_hash,
                               status = 'ready', error = NULL, processed_at = now(),
                               updated_at = now()
                           WHERE rag.documents.storage_provider = 'directory'
                           RETURNING id""",
                        (GLOBAL_NAMESPACE, source, items[0][0].title[:500], content_hash),
                    )
                ).fetchone()
                if row is None:
                    log.warning("knowledge file %s skipped: an upload has that source", source)
                    continue
                await conn.execute("DELETE FROM rag.chunks WHERE document_id = %s", (row[0],))
                await _insert_chunks(
                    conn, row[0], [c for c, _ in items], [v for _, v in items], self._model
                )

    async def search(
        self,
        vector: list[float],
        top_k: int,
        *,
        text: str = "",
        namespaces: Sequence[str] = (),
    ) -> list[RetrievedChunk]:
        if top_k <= 0:
            return []
        self._dimensions.check([vector])
        candidates = max(top_k * _CANDIDATES_PER_RESULT, 20)
        keyword = text.strip()[:_MAX_KEYWORD_QUERY_CHARS]
        params = {
            "vector": vector_literal(vector),
            "namespaces": list(namespaces) or [GLOBAL_NAMESPACE],
            "model": self._model,
            "candidates": candidates,
            # An empty query matches nothing; skip the keyword ranking then.
            "keyword": keyword,
            "keyword_on": bool(keyword) and self._keyword_min_score < 1,
            "k": self._rrf_k,
            "top_k": top_k,
        }
        async with self._pool.connection() as conn, conn.transaction():
            await self._dimensions.verify(conn)
            # Transaction-local: the keyword threshold for `<%`, and HNSW iterative scans
            # so the namespace/model filter cannot starve the index scan (pgvector 0.8).
            await conn.execute(
                "SELECT set_config('pg_trgm.word_similarity_threshold', %s, true),"
                " set_config('hnsw.iterative_scan', 'relaxed_order', true),"
                " set_config('hnsw.ef_search', %s, true)",
                (str(self._keyword_min_score), str(max(40, candidates))),
            )
            cur = conn.cursor(row_factory=dict_row)
            rows = await (await cur.execute(_HYBRID_SEARCH, params)).fetchall()
        return [
            RetrievedChunk(
                Chunk(r["source"], r["title"] or "", r["heading"], r["content"], r["ordinal"]),
                score=float(r["score"]),
                keyword_score=None if r["keyword_score"] is None else float(r["keyword_score"]),
            )
            for r in rows
        ]

    async def count(self) -> int:
        async with self._pool.connection() as conn:
            row = await (
                await conn.execute(
                    "SELECT count(*) FROM rag.chunks WHERE embedding_model = %s", (self._model,)
                )
            ).fetchone()
        return row[0]


# Both rankings filter by namespace and embedding model before ranking. The semantic one
# orders by distance (served by the HNSW index); the keyword one needs `text <% content`
# (GIN trigram index) and orders by word similarity. The fused list is ordered by RRF;
# `score` stays the cosine similarity so the caller's threshold keeps its meaning.
_HYBRID_SEARCH = """
WITH semantic AS (
    SELECT id, row_number() OVER (ORDER BY distance) AS rank
    FROM (
        SELECT c.id, c.embedding <=> %(vector)s::vector AS distance
        FROM rag.chunks c JOIN rag.documents d ON d.id = c.document_id
        WHERE d.namespace = ANY(%(namespaces)s) AND c.embedding_model = %(model)s
        ORDER BY c.embedding <=> %(vector)s::vector
        LIMIT %(candidates)s
    ) nearest
),
keyword AS (
    SELECT id, similarity, row_number() OVER (ORDER BY similarity DESC, id) AS rank
    FROM (
        SELECT c.id, word_similarity(%(keyword)s, c.content) AS similarity
        FROM rag.chunks c JOIN rag.documents d ON d.id = c.document_id
        WHERE %(keyword_on)s AND %(keyword)s <%% c.content
          AND d.namespace = ANY(%(namespaces)s) AND c.embedding_model = %(model)s
        ORDER BY similarity DESC, c.id
        LIMIT %(candidates)s
    ) matches
),
fused AS (
    SELECT id, sum(1.0 / (%(k)s + rank)) AS rrf
    FROM (SELECT id, rank FROM semantic UNION ALL SELECT id, rank FROM keyword) ranked
    GROUP BY id
)
SELECT d.source, d.title, c.heading, c.content, c.ordinal,
       1 - (c.embedding <=> %(vector)s::vector) AS score,
       k.similarity AS keyword_score
FROM fused f
JOIN rag.chunks c ON c.id = f.id
JOIN rag.documents d ON d.id = c.document_id
LEFT JOIN keyword k ON k.id = f.id
ORDER BY f.rrf DESC, c.id
LIMIT %(top_k)s
"""


async def _insert_chunks(
    conn: AsyncConnection,
    document_id: int,
    chunks: Sequence[Chunk],
    vectors: Sequence[Sequence[float]],
    model: str,
) -> None:
    async with conn.cursor() as cur:
        await cur.executemany(
            "INSERT INTO rag.chunks"
            " (document_id, ordinal, heading, content, embedding, embedding_model)"
            " VALUES (%s, %s, %s, %s, %s::vector, %s)",
            [
                (document_id, c.index, c.heading[:2000], c.text, vector_literal(v), model)
                for c, v in zip(chunks, vectors, strict=True)
            ],
        )


_DOCUMENT_COLUMNS = """
    d.id, d.namespace, d.source, d.title, d.status, d.storage_provider, d.storage_bucket,
    d.object_key, d.object_version, d.mime_type, d.size_bytes, d.content_hash, d.error,
    d.created_at, d.updated_at, d.processed_at,
    (SELECT count(*) FROM rag.chunks c WHERE c.document_id = d.id) AS chunks
"""
_JOB_COLUMNS = """
    id, document_id, object_key, object_version, parser_version, chunker_version,
    embedding_model, status, attempts
"""


class PostgresDocumentRepository(DocumentRepository):
    def __init__(self, pool: AsyncConnectionPool, dimensions: int):
        self._pool = pool
        self._dimensions = _DimensionCheck(dimensions)

    async def upsert_pending_upload(
        self, namespace: str, source: str, bucket: str, key: str
    ) -> tuple[int, str | None]:
        async with self._pool.connection() as conn, conn.transaction():
            previous = await (
                await conn.execute(
                    "SELECT object_key, storage_provider FROM rag.documents"
                    " WHERE namespace = %s AND source = %s FOR UPDATE",
                    (namespace, source),
                )
            ).fetchone()
            if previous is not None and previous[1] == "directory":
                raise InvalidInputError("that source is managed by RAG_DOCS_DIR")
            row = await (
                await conn.execute(
                    """INSERT INTO rag.documents
                           (namespace, source, storage_provider, storage_bucket, object_key,
                            status, upload_requested_at)
                       VALUES (%s, %s, 's3', %s, %s, 'pending_upload', now())
                       ON CONFLICT (namespace, source) DO UPDATE
                       SET storage_provider = 's3', storage_bucket = EXCLUDED.storage_bucket,
                           object_key = EXCLUDED.object_key, object_version = NULL,
                           status = 'pending_upload', error = NULL,
                           upload_requested_at = now(), updated_at = now()
                       RETURNING id""",
                    (namespace, source, bucket, key),
                )
            ).fetchone()
        return row[0], previous[0] if previous else None

    async def get(self, namespace: str, document_id: int) -> StoredDocument | None:
        return await self._one(
            f"SELECT {_DOCUMENT_COLUMNS} FROM rag.documents d WHERE d.namespace = %s AND d.id = %s",
            (namespace, document_id),
        )

    async def get_by_id(self, document_id: int) -> StoredDocument | None:
        return await self._one(
            f"SELECT {_DOCUMENT_COLUMNS} FROM rag.documents d WHERE d.id = %s", (document_id,)
        )

    async def find_by_object(self, bucket: str, key: str) -> StoredDocument | None:
        return await self._one(
            f"SELECT {_DOCUMENT_COLUMNS} FROM rag.documents d"
            " WHERE d.storage_bucket = %s AND d.object_key = %s",
            (bucket, key),
        )

    async def list_documents(self, namespace: str, limit: int) -> list[StoredDocument]:
        async with self._pool.connection() as conn:
            cur = conn.cursor(row_factory=dict_row)
            rows = await (
                await cur.execute(
                    f"SELECT {_DOCUMENT_COLUMNS} FROM rag.documents d WHERE d.namespace = %s"
                    " ORDER BY d.updated_at DESC, d.id DESC LIMIT %s",
                    (namespace, limit),
                )
            ).fetchall()
        return [_document(r) for r in rows]

    async def delete(self, namespace: str, document_id: int) -> StoredDocument | None:
        async with self._pool.connection() as conn, conn.transaction():
            cur = conn.cursor(row_factory=dict_row)
            row = await (
                await cur.execute(
                    f"SELECT {_DOCUMENT_COLUMNS} FROM rag.documents d"
                    " WHERE d.namespace = %s AND d.id = %s FOR UPDATE",
                    (namespace, document_id),
                )
            ).fetchone()
            if row is None:
                return None
            # Chunks and jobs go with it (ON DELETE CASCADE).
            await conn.execute("DELETE FROM rag.documents WHERE id = %s", (document_id,))
        return _document(row)

    async def mark_queued(self, document_id: int) -> None:
        async with self._pool.connection() as conn:
            await conn.execute(
                "UPDATE rag.documents SET status = 'queued', error = NULL, updated_at = now()"
                " WHERE id = %s",
                (document_id,),
            )

    async def ensure_job(self, key: JobKey) -> IngestionJob:
        params = (
            key.document_id,
            key.object_key,
            key.object_version,
            key.parser_version,
            key.chunker_version,
            key.embedding_model,
        )
        async with self._pool.connection() as conn:
            cur = conn.cursor(row_factory=dict_row)
            await cur.execute(
                "INSERT INTO rag.ingestion_jobs (document_id, object_key, object_version,"
                " parser_version, chunker_version, embedding_model)"
                " VALUES (%s, %s, %s, %s, %s, %s) ON CONFLICT DO NOTHING",
                params,
            )
            row = await (
                await cur.execute(
                    f"SELECT {_JOB_COLUMNS} FROM rag.ingestion_jobs WHERE document_id = %s"
                    " AND object_key = %s AND object_version = %s AND parser_version = %s"
                    " AND chunker_version = %s AND embedding_model = %s",
                    params,
                )
            ).fetchone()
        return _job(row)

    async def reset_job(self, job_id: int) -> None:
        async with self._pool.connection() as conn:
            await conn.execute(
                "UPDATE rag.ingestion_jobs SET status = 'queued', attempts = 0,"
                " last_error = NULL, started_at = NULL, finished_at = NULL"
                " WHERE id = %s AND status <> 'processing'",
                (job_id,),
            )

    async def claim_job(
        self, job_id: int, lease_seconds: int, max_attempts: int
    ) -> tuple[Claim, IngestionJob | None]:
        async with self._pool.connection() as conn, conn.transaction():
            cur = conn.cursor(row_factory=dict_row)
            document = await (
                await cur.execute(
                    "SELECT d.id, d.object_key FROM rag.documents d"
                    " JOIN rag.ingestion_jobs j ON j.document_id = d.id"
                    " WHERE j.id = %s FOR UPDATE OF d",
                    (job_id,),
                )
            ).fetchone()
            row = await (
                await cur.execute(
                    f"SELECT {_JOB_COLUMNS}, started_at < now() - make_interval(secs => %s)"
                    " AS lease_expired FROM rag.ingestion_jobs WHERE id = %s FOR UPDATE",
                    (lease_seconds, job_id),
                )
            ).fetchone()
            if document is None or row is None:
                return Claim.FINISHED, None
            job = _job(row)
            if job.status in ("succeeded", "failed", "skipped"):
                return Claim.FINISHED, job
            if job.status == "processing" and not row["lease_expired"]:
                return Claim.BUSY, job
            current = document["object_key"] == job.key.object_key
            if job.attempts >= max_attempts:
                await self._finish(
                    conn,
                    job,
                    "failed",
                    f"gave up after {job.attempts} attempts",
                    DocumentStatus.FAILED if current else None,
                )
                return Claim.FINISHED, job
            row = await (
                await cur.execute(
                    "UPDATE rag.ingestion_jobs SET status = 'processing',"
                    " attempts = attempts + 1, started_at = now(), finished_at = NULL"
                    f" WHERE id = %s RETURNING {_JOB_COLUMNS}",
                    (job_id,),
                )
            ).fetchone()
            if current:
                await conn.execute(
                    "UPDATE rag.documents SET status = 'processing', object_version = %s,"
                    " processing_started_at = now(), updated_at = now() WHERE id = %s",
                    (job.key.object_version or None, job.key.document_id),
                )
            return Claim.CLAIMED, _job(row)

    async def latest_success(self, document_id: int) -> tuple[JobKey, str] | None:
        async with self._pool.connection() as conn:
            cur = conn.cursor(row_factory=dict_row)
            row = await (
                await cur.execute(
                    f"SELECT {_JOB_COLUMNS}, content_hash FROM rag.ingestion_jobs"
                    " WHERE document_id = %s AND status = 'succeeded'"
                    " ORDER BY finished_at DESC, id DESC LIMIT 1",
                    (document_id,),
                )
            ).fetchone()
        return (_job(row).key, row["content_hash"]) if row else None

    async def complete_job(
        self,
        job: IngestionJob,
        title: str,
        chunks: Sequence[Chunk],
        vectors: Sequence[list[float]],
        content_hash: str,
        mime_type: str,
        size_bytes: int,
    ) -> bool:
        self._dimensions.check(vectors)
        async with self._pool.connection() as conn, conn.transaction():
            await self._dimensions.verify(conn)
            if not await self._lock_claim(conn, job):
                return False
            if not await self._is_current(conn, job):
                await self._finish(conn, job, "skipped", "superseded by a newer upload", None)
                return False
            await conn.execute(
                "DELETE FROM rag.chunks WHERE document_id = %s", (job.key.document_id,)
            )
            await _insert_chunks(
                conn, job.key.document_id, chunks, vectors, job.key.embedding_model
            )
            await conn.execute(
                """UPDATE rag.documents
                   SET status = 'ready', title = %s, content_hash = %s, mime_type = %s,
                       size_bytes = %s, object_version = %s, error = NULL,
                       processed_at = now(), updated_at = now()
                   WHERE id = %s""",
                (
                    title,
                    content_hash,
                    mime_type,
                    size_bytes,
                    job.key.object_version or None,
                    job.key.document_id,
                ),
            )
            await conn.execute(
                "UPDATE rag.ingestion_jobs SET status = 'succeeded', content_hash = %s,"
                " chunks = %s, last_error = NULL, finished_at = now() WHERE id = %s",
                (content_hash, len(chunks), job.id),
            )
        return True

    async def skip_job(self, job: IngestionJob, reason: str, *, document_ready: bool) -> None:
        async with self._pool.connection() as conn, conn.transaction():
            if not await self._lock_claim(conn, job):
                return
            ready = document_ready and await self._is_current(conn, job)
            await self._finish(
                conn, job, "skipped", reason, DocumentStatus.READY if ready else None
            )

    async def fail_job(self, job: IngestionJob, error: str, *, retry: bool) -> None:
        async with self._pool.connection() as conn, conn.transaction():
            if not await self._lock_claim(conn, job):
                return
            status = DocumentStatus.QUEUED if retry else DocumentStatus.FAILED
            current = await self._is_current(conn, job)
            await self._finish(
                conn, job, "queued" if retry else "failed", error, status if current else None
            )

    async def _one(self, query: str, params: tuple) -> StoredDocument | None:
        async with self._pool.connection() as conn:
            cur = conn.cursor(row_factory=dict_row)
            row = await (await cur.execute(query, params)).fetchone()
        return _document(row) if row else None

    @staticmethod
    async def _lock_claim(conn: AsyncConnection, job: IngestionJob) -> bool:
        """Lock the document, then the job; True when this attempt still holds the job."""
        await conn.execute(
            "SELECT 1 FROM rag.documents WHERE id = %s FOR UPDATE", (job.key.document_id,)
        )
        row = await (
            await conn.execute(
                "SELECT status, attempts FROM rag.ingestion_jobs WHERE id = %s FOR UPDATE",
                (job.id,),
            )
        ).fetchone()
        held = row is not None and row[0] == "processing" and row[1] == job.attempts
        if not held:
            log.info("ingestion job %d attempt %d lost its claim", job.id, job.attempts)
        return held

    @staticmethod
    async def _is_current(conn: AsyncConnection, job: IngestionJob) -> bool:
        """The document still points at the job's object (no newer upload)."""
        row = await (
            await conn.execute(
                "SELECT object_key FROM rag.documents WHERE id = %s", (job.key.document_id,)
            )
        ).fetchone()
        return row is not None and row[0] == job.key.object_key

    @staticmethod
    async def _finish(
        conn: AsyncConnection,
        job: IngestionJob,
        status: str,
        error: str,
        document_status: DocumentStatus | None,
    ) -> None:
        finished = status != "queued"
        await conn.execute(
            "UPDATE rag.ingestion_jobs SET status = %s, last_error = %s,"
            " finished_at = CASE WHEN %s THEN now() END WHERE id = %s",
            (status, error[:2000], finished, job.id),
        )
        if document_status is None:
            return
        ready = document_status == DocumentStatus.READY
        await conn.execute(
            "UPDATE rag.documents SET status = %s, error = %s, updated_at = now(),"
            " processed_at = CASE WHEN %s THEN now() ELSE processed_at END,"
            " object_version = CASE WHEN %s THEN %s ELSE object_version END"
            " WHERE id = %s",
            (
                document_status.value,
                None if ready else error[:2000],
                ready,
                ready,
                job.key.object_version or None,
                job.key.document_id,
            ),
        )


def _document(row: dict) -> StoredDocument:
    return StoredDocument(
        id=row["id"],
        namespace=row["namespace"],
        source=row["source"],
        title=row["title"],
        status=DocumentStatus(row["status"]),
        storage_provider=row["storage_provider"],
        storage_bucket=row["storage_bucket"],
        object_key=row["object_key"],
        object_version=row["object_version"],
        mime_type=row["mime_type"],
        size_bytes=row["size_bytes"],
        content_hash=row["content_hash"],
        error=row["error"],
        chunks=row["chunks"],
        created_at=row["created_at"],
        updated_at=row["updated_at"],
        processed_at=row["processed_at"],
    )


def _job(row: dict) -> IngestionJob:
    return IngestionJob(
        id=row["id"],
        key=JobKey(
            document_id=row["document_id"],
            object_key=row["object_key"],
            object_version=row["object_version"],
            parser_version=row["parser_version"],
            chunker_version=row["chunker_version"],
            embedding_model=row["embedding_model"],
        ),
        status=row["status"],
        attempts=row["attempts"],
    )
