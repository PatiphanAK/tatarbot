"""Outbound ports. Adapters implement these; the application only sees them."""

from abc import ABC, abstractmethod
from collections.abc import Sequence

from agent_runtime.domain import (
    ChatMessage,
    Chunk,
    Document,
    InferenceRequest,
    InferenceResult,
    RetrievedChunk,
    SearchResult,
    StoredDocument,
)
from agent_runtime.domain.ingestion import (
    Claim,
    IngestionJob,
    JobKey,
    PresignedUpload,
    StoredObject,
)


class ModelPort(ABC):
    """Provider-agnostic text generation (vLLM, Ollama, OpenAI, a queue...).

    Implementations raise subclasses of `ModelError` on failure.
    """

    @abstractmethod
    async def generate(self, request: InferenceRequest) -> InferenceResult: ...

    @abstractmethod
    async def is_ready(self) -> bool:
        """Cheap check that the backend can currently serve requests."""

    async def aclose(self) -> None:  # noqa: B027 - optional hook
        """Release resources (connections...)."""


class ConversationStore(ABC):
    """Persists conversation history (system prompt excluded).

    In-memory today; Redis/Postgres later without touching `ChatService`.
    """

    @abstractmethod
    async def load(self, conversation_id: str) -> list[ChatMessage]:
        """Return the stored messages, oldest first ([] if unknown)."""

    @abstractmethod
    async def append(self, conversation_id: str, messages: Sequence[ChatMessage]) -> None: ...

    @abstractmethod
    async def replace_last(self, conversation_id: str, messages: Sequence[ChatMessage]) -> bool:
        """Replace the last `len(messages)` stored messages if their roles match; False
        otherwise. Used to record the outcome of client-side actions after the fact."""


class SearchPort(ABC):
    """Web search (Tavily today). Implementations raise `SearchError` on failure."""

    @abstractmethod
    async def search(self, query: str, max_results: int) -> list[SearchResult]: ...

    async def aclose(self) -> None:  # noqa: B027 - optional hook
        """Release resources (connections...)."""


class EmbeddingPort(ABC):
    """Text -> vector, for any embedding model (an OpenAI-compatible server, a local
    hashing embedder...). Implementations raise `EmbeddingError` on failure.

    Queries and documents are separate calls because many models embed them differently
    (an instruction or `query:` / `passage:` prefix).
    """

    @property
    @abstractmethod
    def identity(self) -> str:
        """Names the vector space: two embedders with the same identity produce comparable
        vectors. It must change with the model, dimensions or prefixes; stored vectors
        with another identity are discarded."""

    @abstractmethod
    async def embed_documents(self, texts: Sequence[str]) -> list[list[float]]:
        """One vector per text, in order."""

    @abstractmethod
    async def embed_query(self, text: str) -> list[float]: ...

    async def aclose(self) -> None:  # noqa: B027 - optional hook
        """Release resources (connections...)."""


class VectorStore(ABC):
    """Holds the embedded chunks and finds the nearest ones (in memory, or pgvector)."""

    # True when other processes add chunks (the ingestion worker): the store may hold
    # chunks although this process has indexed nothing.
    shared: bool = False

    @abstractmethod
    async def replace(self, chunks: Sequence[Chunk], vectors: Sequence[list[float]]) -> None:
        """Swap the indexed documents (RAG_DOCS_DIR's) atomically; searches never see a
        half-built index."""

    @abstractmethod
    async def search(
        self,
        vector: list[float],
        top_k: int,
        *,
        text: str = "",
        namespaces: Sequence[str] = (),
    ) -> list[RetrievedChunk]:
        """Up to `top_k` chunks, best first. A store may also match `text` by keywords and
        fuse both rankings; one that cannot ignores it. Stores that scope documents only
        return those in `namespaces`."""

    @abstractmethod
    async def count(self) -> int: ...

    async def aclose(self) -> None:  # noqa: B027 - optional hook
        """Release resources (connections...)."""


class DocumentSource(ABC):
    """Where knowledge documents come from (a directory today)."""

    @abstractmethod
    async def load(self) -> list[Document]: ...


class ObjectStorage(ABC):
    """The private bucket uploads go to (S3). Raises `StorageError` on transient
    failures."""

    @property
    @abstractmethod
    def bucket(self) -> str: ...

    @abstractmethod
    def presign_upload(self, key: str, max_bytes: int, expires_seconds: int) -> PresignedUpload:
        """A browser-style POST upload of exactly `key`, at most `max_bytes`, enforced by
        the storage service."""

    @abstractmethod
    async def read(self, key: str, version: str | None, max_bytes: int) -> StoredObject:
        """The object's bytes. Raises `InvalidDocumentError` when it is larger than
        `max_bytes` (without downloading it) and `ObjectMissingError` when it is gone."""

    @abstractmethod
    async def delete(self, key: str) -> None: ...


class IngestQueue(ABC):
    """Where ingestion requests go (the SQS queue S3 events also arrive on)."""

    @abstractmethod
    async def send(self, message: dict) -> None: ...


class DocumentRepository(ABC):
    """Uploaded documents and their ingestion jobs (Postgres).

    Every method that takes a namespace only sees documents in it: a caller cannot reach
    another namespace's document by guessing its id.
    """

    @abstractmethod
    async def upsert_pending_upload(
        self, namespace: str, source: str, bucket: str, key: str
    ) -> tuple[int, str | None]:
        """Create the document, or point an existing one (same namespace and source) at a
        new object. Returns (id, the previous object key or None)."""

    @abstractmethod
    async def get(self, namespace: str, document_id: int) -> StoredDocument | None: ...

    @abstractmethod
    async def get_by_id(self, document_id: int) -> StoredDocument | None:
        """For the worker only: requests on the queue come from the runtime itself."""

    @abstractmethod
    async def find_by_object(self, bucket: str, key: str) -> StoredDocument | None: ...

    @abstractmethod
    async def list_documents(self, namespace: str, limit: int) -> list[StoredDocument]: ...

    @abstractmethod
    async def delete(self, namespace: str, document_id: int) -> StoredDocument | None:
        """Delete the document, its chunks and jobs; returns what was deleted."""

    @abstractmethod
    async def mark_queued(self, document_id: int) -> None: ...

    @abstractmethod
    async def ensure_job(self, key: JobKey) -> IngestionJob:
        """The job for this object and pipeline configuration, created if new."""

    @abstractmethod
    async def reset_job(self, job_id: int) -> None:
        """Make a finished job runnable again, with its attempts reset."""

    @abstractmethod
    async def claim_job(
        self, job_id: int, lease_seconds: int, max_attempts: int
    ) -> tuple[Claim, IngestionJob | None]:
        """Start an attempt: the job must be queued, or processing with an expired lease
        (a crashed worker). A job out of attempts is failed instead (FINISHED). On CLAIMED
        the returned job carries this attempt's number and the document is processing."""

    @abstractmethod
    async def latest_success(self, document_id: int) -> tuple[JobKey, str] | None:
        """The configuration and content hash of the job whose chunks are live."""

    @abstractmethod
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
        """In one transaction: replace the document's chunks, mark it ready and the job
        succeeded. False (and nothing written) when the attempt lost its claim or the
        document now points at another object (a newer upload)."""

    @abstractmethod
    async def skip_job(self, job: IngestionJob, reason: str, *, document_ready: bool) -> None:
        """Finish the job without new chunks: superseded, or content already indexed (then
        `document_ready` marks the document ready again)."""

    @abstractmethod
    async def fail_job(self, job: IngestionJob, error: str, *, retry: bool) -> None:
        """Record a failed attempt. With `retry` the job is queued again; without, the job
        and the document are failed. The document's live chunks are kept either way."""
