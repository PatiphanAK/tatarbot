"""Transport- and provider-independent types.

Nothing in here may import FastAPI, httpx, or know about Discord or vLLM.
"""

from agent_runtime.domain import guardrails
from agent_runtime.domain.errors import (
    EmbeddingError,
    InvalidInputError,
    ModelError,
    ModelRequestRejectedError,
    ModelResponseError,
    ModelTimeoutError,
    ModelUnavailableError,
    SearchError,
)
from agent_runtime.domain.inference import (
    ChatMessage,
    GenerationParams,
    InferenceRequest,
    InferenceResult,
    Role,
    TokenUsage,
    ToolCall,
    ToolSpec,
)
from agent_runtime.domain.knowledge import Chunk, Document, RetrievedChunk
from agent_runtime.domain.language import Language, detect_language
from agent_runtime.domain.search import SearchResult

__all__ = [
    "ChatMessage",
    "Chunk",
    "Document",
    "EmbeddingError",
    "GenerationParams",
    "InferenceRequest",
    "InferenceResult",
    "InvalidInputError",
    "Language",
    "ModelError",
    "ModelRequestRejectedError",
    "ModelResponseError",
    "ModelTimeoutError",
    "ModelUnavailableError",
    "RetrievedChunk",
    "Role",
    "SearchError",
    "SearchResult",
    "TokenUsage",
    "ToolCall",
    "ToolSpec",
    "detect_language",
    "guardrails",
]
