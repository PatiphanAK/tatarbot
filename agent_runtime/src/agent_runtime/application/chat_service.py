"""Chat use case: history + system prompt -> InferenceRequest -> ModelPort."""

import asyncio
import logging
import time
import weakref
from dataclasses import dataclass, field

from agent_runtime.application.ports import ConversationStore, ModelPort
from agent_runtime.domain import (
    ChatMessage,
    GenerationParams,
    InferenceRequest,
    InvalidInputError,
    ModelResponseError,
    Role,
)

log = logging.getLogger(__name__)

MAX_CONVERSATION_ID_LENGTH = 256


@dataclass(frozen=True)
class ChatSettings:
    system_prompt: str
    # Past messages (user + assistant) sent to the model with each turn.
    max_history_messages: int = 20
    generation: GenerationParams = field(default_factory=GenerationParams)


class ChatService:
    def __init__(self, model: ModelPort, store: ConversationStore, settings: ChatSettings):
        self._model = model
        self._store = store
        self._settings = settings
        # Serialises turns within one conversation so history stays ordered.
        # Per-process only; a shared store with several replicas would need
        # a distributed lock or optimistic concurrency.
        self._locks: weakref.WeakValueDictionary[str, asyncio.Lock] = weakref.WeakValueDictionary()

    async def chat(self, conversation_id: str, message: str) -> str:
        conversation_id = conversation_id.strip()
        message = message.strip()
        if not conversation_id or len(conversation_id) > MAX_CONVERSATION_ID_LENGTH:
            raise InvalidInputError("conversation_id must be 1-256 characters")
        if not message:
            raise InvalidInputError("message must not be empty")

        async with self._lock_for(conversation_id):
            history = await self._store.load(conversation_id)
            user = ChatMessage(Role.USER, message)
            request = InferenceRequest(
                messages=(
                    ChatMessage(Role.SYSTEM, self._settings.system_prompt),
                    *self._recent(history),
                    user,
                ),
                params=self._settings.generation,
            )

            started = time.perf_counter()
            result = await self._model.generate(request)
            reply = result.content.strip()
            if not reply:
                raise ModelResponseError("model returned an empty reply")

            # Only successful exchanges become history.
            await self._store.append(conversation_id, [user, ChatMessage(Role.ASSISTANT, reply)])

        log.info(
            "chat turn completed conversation_id=%s request_id=%s history=%d "
            "latency_ms=%d finish_reason=%s",
            conversation_id,
            request.request_id,
            len(request.messages) - 2,
            round((time.perf_counter() - started) * 1000),
            result.finish_reason,
        )
        return reply

    def _recent(self, history: list[ChatMessage]) -> list[ChatMessage]:
        limit = self._settings.max_history_messages
        if limit <= 0:
            return []
        recent = history[-limit:]
        # Chat templates expect user/assistant alternation starting with user.
        while recent and recent[0].role is not Role.USER:
            recent = recent[1:]
        return recent

    def _lock_for(self, conversation_id: str) -> asyncio.Lock:
        lock = self._locks.get(conversation_id)
        if lock is None:
            lock = asyncio.Lock()
            self._locks[conversation_id] = lock
        return lock
