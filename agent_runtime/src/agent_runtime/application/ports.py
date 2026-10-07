"""Outbound ports. Adapters implement these; the application only sees them."""

from abc import ABC, abstractmethod
from collections.abc import Sequence

from agent_runtime.domain import ChatMessage, InferenceRequest, InferenceResult


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
