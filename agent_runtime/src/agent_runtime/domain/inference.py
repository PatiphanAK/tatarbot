"""The generic inference contract.

`InferenceRequest` is what any transport (HTTP today; a queue/scheduler and
GPU workers later) hands to a `ModelPort`. It deliberately carries no model
name, URL or provider-specific option: those belong to the adapter.
"""

from dataclasses import dataclass, field
from enum import StrEnum
from uuid import uuid4


class Role(StrEnum):
    SYSTEM = "system"
    USER = "user"
    ASSISTANT = "assistant"


@dataclass(frozen=True, slots=True)
class ChatMessage:
    role: Role
    content: str


@dataclass(frozen=True, slots=True)
class GenerationParams:
    max_tokens: int | None = None
    temperature: float | None = None


@dataclass(frozen=True, slots=True)
class InferenceRequest:
    messages: tuple[ChatMessage, ...]
    params: GenerationParams = field(default_factory=GenerationParams)
    request_id: str = field(default_factory=lambda: uuid4().hex)


@dataclass(frozen=True, slots=True)
class TokenUsage:
    prompt_tokens: int
    completion_tokens: int


@dataclass(frozen=True, slots=True)
class InferenceResult:
    content: str
    finish_reason: str | None = None
    usage: TokenUsage | None = None
