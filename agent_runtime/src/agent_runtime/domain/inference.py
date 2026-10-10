"""The generic inference contract.

`InferenceRequest` is what any transport (HTTP today; a queue/scheduler and
GPU workers later) hands to a `ModelPort`. It deliberately carries no model
name, URL or provider-specific option: those belong to the adapter.
"""

from dataclasses import dataclass, field
from enum import StrEnum
from typing import Any
from uuid import uuid4


class Role(StrEnum):
    SYSTEM = "system"
    USER = "user"
    ASSISTANT = "assistant"
    TOOL = "tool"


@dataclass(frozen=True, slots=True)
class ToolCall:
    """A function call requested by the model. `arguments` is the raw JSON string."""

    id: str
    name: str
    arguments: str


@dataclass(frozen=True, slots=True)
class ToolSpec:
    """A function the model may call; `parameters` is a JSON Schema object."""

    name: str
    description: str
    parameters: dict[str, Any]


@dataclass(frozen=True, slots=True)
class ChatMessage:
    role: Role
    content: str
    # Assistant messages that call tools (content may then be empty).
    tool_calls: tuple[ToolCall, ...] = ()
    # Tool messages: the call this result answers.
    tool_call_id: str | None = None


@dataclass(frozen=True, slots=True)
class GenerationParams:
    max_tokens: int | None = None
    temperature: float | None = None


@dataclass(frozen=True, slots=True)
class InferenceRequest:
    messages: tuple[ChatMessage, ...]
    params: GenerationParams = field(default_factory=GenerationParams)
    # Offered to the model; empty means plain chat.
    tools: tuple[ToolSpec, ...] = ()
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
    tool_calls: tuple[ToolCall, ...] = ()
