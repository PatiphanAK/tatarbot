"""Uploaded documents: presigned upload, asynchronous ingestion, reprocessing, deletion.

    client -> POST /v1/knowledge/uploads -> Uploads.create: pending row + presigned POST
    client -> S3 (direct, size enforced by S3) -> S3 event -> SQS
    worker -> Ingestor.handle_object_created: claim job -> read -> parse -> chunk -> embed
           -> one transaction: replace the document's chunks, mark it ready

Idempotency: an ingestion job is identified by (document, object key, object version,
parser, chunker, embedding model); a duplicate event finds the finished job and does
nothing. Content that is already indexed under the same configuration is not re-embedded.

Failures: content that can never be ingested (wrong type, too large, empty) fails the job
at once; anything else (embedding backend, S3, database) is retried by SQS redelivery
until RAG_INGEST_MAX_ATTEMPTS, then fails. A crashed worker's job is taken over when its
lease expires. Retrieval never sees a partial document: chunks are only replaced whole,
and a failed run keeps the previous complete version live.
"""

import enum
import hashlib
import logging
import time
import uuid
from dataclasses import dataclass
from pathlib import PurePosixPath

from agent_runtime.application.knowledge import KnowledgeSettings
from agent_runtime.application.ports import (
    DocumentRepository,
    EmbeddingPort,
    IngestQueue,
    ObjectStorage,
)
from agent_runtime.domain import (
    Document,
    EmbeddingError,
    InvalidDocumentError,
    ObjectMissingError,
    StorageError,
    StoredDocument,
)
from agent_runtime.domain.ingestion import (
    PARSER_VERSION,
    Claim,
    IngestionJob,
    JobKey,
    PresignedUpload,
    check_size,
    parse_document,
    title_of,
    upload_source,
    validate_namespace,
)
from agent_runtime.domain.knowledge import chunk_document

log = logging.getLogger(__name__)

REPROCESS_MESSAGE = "reprocess"


@dataclass(frozen=True)
class UploadSettings:
    max_bytes: int = 1_000_000
    url_ttl_seconds: int = 300
    # Object keys are `<prefix><random hex><extension>`; nothing from the client.
    key_prefix: str = "rag/"
    max_attempts: int = 5
    lease_seconds: int = 300


class Outcome(enum.Enum):
    # Finished one way or another: acknowledge the message.
    DONE = "done"
    # Try again later: leave the message for redelivery.
    RETRY = "retry"


class Uploads:
    """The upload API's use cases. Every call is scoped to one namespace."""

    def __init__(
        self,
        documents: DocumentRepository,
        storage: ObjectStorage,
        queue: IngestQueue,
        settings: UploadSettings | None = None,
    ):
        self._documents = documents
        self._storage = storage
        self._queue = queue
        self._settings = settings or UploadSettings()

    @property
    def settings(self) -> UploadSettings:
        return self._settings

    async def create(
        self, namespace: str, filename: str, size_bytes: int, source: str | None = None
    ) -> tuple[StoredDocument, PresignedUpload]:
        """A pending document and a presigned upload for it. Asking again for the same
        source issues a new object key: only the latest upload is ingested, and the
        previous version stays retrievable until the new one is ready."""
        validate_namespace(namespace)
        source, extension = upload_source(filename, source)
        check_size(size_bytes, self._settings.max_bytes)
        key = f"{self._settings.key_prefix}{uuid.uuid4().hex}{extension}"
        document_id, previous = await self._documents.upsert_pending_upload(
            namespace, source, self._storage.bucket, key
        )
        upload = self._storage.presign_upload(
            key, self._settings.max_bytes, self._settings.url_ttl_seconds
        )
        if previous and previous != key:
            await self._delete_object(previous)
        document = await self._documents.get(namespace, document_id)
        log.info(
            "upload requested document_id=%d namespace=%s size=%d replaces=%s",
            document_id,
            namespace,
            size_bytes,
            bool(previous),
        )
        return document, upload

    async def get(self, namespace: str, document_id: int) -> StoredDocument | None:
        return await self._documents.get(validate_namespace(namespace), document_id)

    async def list_documents(self, namespace: str, limit: int = 100) -> list[StoredDocument]:
        return await self._documents.list_documents(validate_namespace(namespace), limit)

    async def delete(self, namespace: str, document_id: int) -> bool:
        """Delete the document (its chunks leave retrieval at once), then its object."""
        document = await self._documents.delete(validate_namespace(namespace), document_id)
        if document is None:
            return False
        if document.storage_provider == "s3" and document.object_key:
            await self._delete_object(document.object_key)
        log.info("document deleted document_id=%d namespace=%s", document_id, namespace)
        return True

    async def reprocess(self, namespace: str, document_id: int, force: bool = False) -> bool:
        """Queue the document for ingestion again. Without `force` it is a no-op unless the
        pipeline configuration changed (e.g. a new embedding model); with it, a failed or
        finished job runs again."""
        document = await self._documents.get(validate_namespace(namespace), document_id)
        if document is None or document.storage_provider != "s3":
            return False
        await self._documents.mark_queued(document_id)
        await self._queue.send(
            {"type": REPROCESS_MESSAGE, "document_id": document_id, "force": force}
        )
        log.info("reprocess queued document_id=%d force=%s", document_id, force)
        return True

    async def _delete_object(self, key: str) -> None:
        # Best effort: an orphaned object costs storage, not correctness (its events are
        # ignored because no document points at it).
        try:
            await self._storage.delete(key)
        except StorageError as e:
            log.warning("could not delete upload object %s: %s", key, e)


class Ingestor:
    """Turns an uploaded object into a ready document. Called by the queue worker."""

    def __init__(
        self,
        documents: DocumentRepository,
        storage: ObjectStorage,
        embedder: EmbeddingPort,
        knowledge: KnowledgeSettings,
        settings: UploadSettings | None = None,
    ):
        self._documents = documents
        self._storage = storage
        self._embedder = embedder
        self._knowledge = knowledge
        self._settings = settings or UploadSettings()

    async def handle_object_created(self, bucket: str, key: str, version: str) -> Outcome:
        if bucket != self._storage.bucket:
            log.warning("ignoring event for bucket %s", bucket)
            return Outcome.DONE
        document = await self._documents.find_by_object(bucket, key)
        if document is None:
            # Replaced by a newer upload, deleted, or not ours: nothing to do.
            log.info("ignoring event for unknown object %s", key)
            return Outcome.DONE
        return await self._ingest(document, version, force=False)

    async def reprocess(self, document_id: int, force: bool) -> Outcome:
        document = await self._documents.get_by_id(document_id)
        if document is None or not document.object_key:
            log.info("reprocess: document %d is gone", document_id)
            return Outcome.DONE
        if document.object_version is None:
            # No upload event has been processed yet; that event will ingest it.
            log.info("reprocess: document %d has not been uploaded yet", document_id)
            return Outcome.DONE
        return await self._ingest(document, document.object_version, force=force)

    async def _ingest(self, document: StoredDocument, version: str, force: bool) -> Outcome:
        key = JobKey(
            document_id=document.id,
            object_key=document.object_key,
            object_version=version,
            parser_version=PARSER_VERSION,
            chunker_version=self._knowledge.chunker_version,
            embedding_model=self._embedder.identity,
        )
        job = await self._documents.ensure_job(key)
        if force:
            await self._documents.reset_job(job.id)
        claim, job = await self._documents.claim_job(
            job.id, self._settings.lease_seconds, self._settings.max_attempts
        )
        if claim is Claim.FINISHED:
            log.info("ingestion job %s already finished", job.id if job else "?")
            return Outcome.DONE
        if claim is Claim.BUSY:
            return Outcome.RETRY

        started = time.perf_counter()
        try:
            chunks = await self._run(document, job)
        except InvalidDocumentError as e:
            log.info("ingestion rejected document_id=%d: %s", document.id, e)
            await self._documents.fail_job(job, str(e), retry=False)
            return Outcome.DONE
        except ObjectMissingError as e:
            log.info("ingestion: object gone document_id=%d: %s", document.id, e)
            await self._documents.fail_job(job, "the uploaded file is missing", retry=False)
            return Outcome.DONE
        except Exception as e:
            retry = job.attempts < self._settings.max_attempts
            if isinstance(e, EmbeddingError | StorageError):
                log.warning(
                    "ingestion failed document_id=%d attempt=%d retry=%s: %s",
                    document.id,
                    job.attempts,
                    retry,
                    e,
                )
                error = f"{type(e).__name__}: {e}"
            else:
                log.exception(
                    "ingestion failed document_id=%d attempt=%d", document.id, job.attempts
                )
                error = "internal error"
            await self._documents.fail_job(job, error, retry=retry)
            return Outcome.RETRY if retry else Outcome.DONE
        log.info(
            "ingestion finished document_id=%d job_id=%d attempt=%d chunks=%s duration_ms=%d",
            document.id,
            job.id,
            job.attempts,
            chunks,
            round((time.perf_counter() - started) * 1000),
        )
        return Outcome.DONE

    async def _run(self, document: StoredDocument, job: IngestionJob) -> int | None:
        """One attempt; the number of chunks written, None when nothing changed."""
        max_bytes = self._settings.max_bytes
        version = job.key.object_version or None
        stored = await self._storage.read(job.key.object_key, version, max_bytes)
        # The key ends in the extension the upload was accepted with.
        parsed = parse_document(stored.data, PurePosixPath(job.key.object_key).suffix, max_bytes)
        content_hash = hashlib.sha256(stored.data).hexdigest()

        live = await self._documents.latest_success(document.id)
        if live is not None and live[1] == content_hash and _same_pipeline(live[0], job.key):
            await self._documents.skip_job(job, "content already indexed", document_ready=True)
            return None

        title = title_of(parsed, document.source)
        s = self._knowledge
        chunks = chunk_document(
            Document(document.source, title, parsed.text), s.chunk_chars, s.chunk_overlap
        )
        if not chunks:
            raise InvalidDocumentError("the file has no text to index")
        vectors = await self._embedder.embed_documents([c.embedding_text for c in chunks])
        if len(vectors) != len(chunks):
            raise EmbeddingError("embedder returned the wrong number of vectors")
        written = await self._documents.complete_job(
            job, title, chunks, vectors, content_hash, parsed.mime_type, len(stored.data)
        )
        return len(chunks) if written else None


def _same_pipeline(a: JobKey, b: JobKey) -> bool:
    return (a.parser_version, a.chunker_version, a.embedding_model) == (
        b.parser_version,
        b.chunker_version,
        b.embedding_model,
    )
