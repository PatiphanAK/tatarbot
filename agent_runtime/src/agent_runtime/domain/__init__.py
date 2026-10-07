"""Transport- and provider-independent types.

Nothing in here may import FastAPI, httpx, or know about Discord or vLLM.
"""

from agent_runtime.domain.errors import (
    InvalidInputError,
    ModelError,
    ModelRequestRejectedError,
    ModelResponseError,
    ModelTimeoutError,
    ModelUnavailableError,
)
from agent_runtime.domain.inference import (
    ChatMessage,
    GenerationParams,
    InferenceRequest,
    InferenceResult,
    Role,
    TokenUsage,
)

__all__ = [
    "ChatMessage",
    "GenerationParams",
    "InferenceRequest",
    "InferenceResult",
    "InvalidInputError",
    "ModelError",
    "ModelRequestRejectedError",
    "ModelResponseError",
    "ModelTimeoutError",
    "ModelUnavailableError",
    "Role",
    "TokenUsage",
]
