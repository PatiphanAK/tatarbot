"""Transport- and provider-independent types.

Nothing in here may import FastAPI, httpx, or know about Discord or vLLM.
"""

from agent_runtime.domain import guardrails
from agent_runtime.domain.errors import (
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
from agent_runtime.domain.language import Language, detect_language
from agent_runtime.domain.search import SearchResult

__all__ = [
    "ChatMessage",
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
    "Role",
    "SearchError",
    "SearchResult",
    "TokenUsage",
    "ToolCall",
    "ToolSpec",
    "detect_language",
    "guardrails",
]
