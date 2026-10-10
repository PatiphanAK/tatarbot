"""Chat use case: history + system prompt -> SearchAgent (ModelPort + SearchPort).

Around the agent: the reply language is detected per turn and stated in the system
message, and the guardrails in `agent_runtime.domain.guardrails` check input and output.
Only the user message and the final reply are stored; search steps are not.
"""

import asyncio
import logging
import time
import weakref
from dataclasses import dataclass, field
from uuid import uuid4

from agent_runtime.application.agent import AgentSettings, SearchAgent
from agent_runtime.application.ports import ConversationStore, ModelPort, SearchPort
from agent_runtime.domain import (
    ChatMessage,
    GenerationParams,
    InvalidInputError,
    Language,
    ModelResponseError,
    Role,
    SearchResult,
    detect_language,
    guardrails,
)
from agent_runtime.domain.language import contains_thai, language_directive

log = logging.getLogger(__name__)

MAX_CONVERSATION_ID_LENGTH = 256
MAX_SOURCES_SHOWN = 3
_SOURCES_LABEL = {Language.THAI: "แหล่งที่มา", Language.ENGLISH: "Sources"}


@dataclass(frozen=True)
class ChatSettings:
    system_prompt: str
    # Past messages (user + assistant) sent to the model with each turn.
    max_history_messages: int = 20
    generation: GenerationParams = field(default_factory=GenerationParams)
    guardrails_enabled: bool = True
    # Web search (only with a SearchPort): searches per message, results per search.
    max_searches: int = 3
    results_per_search: int = 5
    # Whole turn, all model calls and searches. Below the caller's timeout (bot: 120 s).
    turn_timeout_seconds: float = 110.0


class ChatService:
    def __init__(
        self,
        model: ModelPort,
        store: ConversationStore,
        settings: ChatSettings,
        search: SearchPort | None = None,
    ):
        self._store = store
        self._settings = settings
        self._agent = SearchAgent(
            model,
            search,
            AgentSettings(
                generation=settings.generation,
                max_searches=settings.max_searches,
                results_per_search=settings.results_per_search,
                turn_timeout_seconds=settings.turn_timeout_seconds,
            ),
        )
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

        guarded = self._settings.guardrails_enabled
        if guarded and (reason := guardrails.check_input(message)):
            # Refused turns never reach the model and are not stored.
            log.warning("input refused conversation_id=%s reason=%s", conversation_id, reason)
            return guardrails.refusal(detect_language(message))

        async with self._lock_for(conversation_id):
            history = await self._store.load(conversation_id)
            recent = self._recent(history)
            language = detect_language(message) or _conversation_language(recent)
            user = ChatMessage(Role.USER, message)
            messages = (
                ChatMessage(Role.SYSTEM, self._system_message(language)),
                *recent,
                user,
            )
            request_id = uuid4().hex

            started = time.perf_counter()
            result = await self._agent.run(messages, request_id)
            reply = result.content.strip()
            if not reply:
                raise ModelResponseError("model returned an empty reply")

            if guarded:
                if guardrails.leaks_prompt(reply, self._settings.system_prompt):
                    log.warning(
                        "reply withheld conversation_id=%s request_id=%s reason=prompt_leak",
                        conversation_id,
                        request_id,
                    )
                    return guardrails.refusal(language)
                reply, redacted = guardrails.redact_secrets(reply)
                if redacted:
                    log.warning(
                        "reply redacted conversation_id=%s request_id=%s secrets=%d",
                        conversation_id,
                        request_id,
                        redacted,
                    )

            # Only successful exchanges become history, without the sources footer.
            await self._store.append(conversation_id, [user, ChatMessage(Role.ASSISTANT, reply)])

        if language is Language.THAI and not contains_thai(reply):
            log.warning(
                "reply language mismatch conversation_id=%s request_id=%s expected=th",
                conversation_id,
                request_id,
            )
        log.info(
            "chat turn completed conversation_id=%s request_id=%s history=%d language=%s "
            "react_steps=%d searches=%d model_calls=%d latency_ms=%d finish_reason=%s",
            conversation_id,
            request_id,
            len(messages) - 2,
            language or "-",
            len(result.steps),
            result.searches,
            result.model_calls,
            round((time.perf_counter() - started) * 1000),
            result.finish_reason,
        )
        return reply + _sources_footer(result.sources, reply, language)

    def _system_message(self, language: Language | None) -> str:
        parts = [self._settings.system_prompt.rstrip()]
        if search := self._agent.instructions():
            parts.append(search)
        if language is not None:
            parts.append(language_directive(language))
        return "\n\n".join(parts)

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


def _conversation_language(history: list[ChatMessage]) -> Language | None:
    """Language of the latest user message that has one (for "ok", "555", emoji...)."""
    for m in reversed(history):
        if m.role is Role.USER and (language := detect_language(m.content)):
            return language
    return None


def _sources_footer(
    sources: tuple[SearchResult, ...], reply: str, language: Language | None
) -> str:
    """Links from the actual search results (not from the model), unless already cited.

    `<url>` stops Discord from unfurling every link into an embed.
    """
    shown = [s.url for s in sources if s.url not in reply][:MAX_SOURCES_SHOWN]
    if not shown:
        return ""
    label = _SOURCES_LABEL[language or Language.ENGLISH]
    return f"\n\n{label}:\n" + "\n".join(f"- <{url}>" for url in shown)
