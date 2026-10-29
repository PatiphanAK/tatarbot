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
    """Holds the embedded chunks and finds the nearest ones.

    In-memory today; pgvector or a vector database later without touching the pipeline.
    """

    @abstractmethod
    async def replace(self, chunks: Sequence[Chunk], vectors: Sequence[list[float]]) -> None:
        """Swap the whole index atomically; searches never see a half-built index."""

    @abstractmethod
    async def search(self, vector: list[float], top_k: int) -> list[RetrievedChunk]:
        """Up to `top_k` chunks, closest first."""

    @abstractmethod
    async def count(self) -> int: ...


class DocumentSource(ABC):
    """Where knowledge documents come from (a directory today)."""

    @abstractmethod
    async def load(self) -> list[Document]: ...
