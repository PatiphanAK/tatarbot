"""In-memory `ConversationStore`. History is lost on restart."""

from collections import OrderedDict
from collections.abc import Sequence

from agent_runtime.application.ports import ConversationStore
from agent_runtime.domain import ChatMessage


class InMemoryConversationStore(ConversationStore):
    def __init__(self, max_messages_per_conversation: int = 100, max_conversations: int = 10_000):
        self._max_messages = max_messages_per_conversation
        self._max_conversations = max_conversations
        # Ordered by last use, for LRU eviction.
        self._data: OrderedDict[str, list[ChatMessage]] = OrderedDict()

    async def load(self, conversation_id: str) -> list[ChatMessage]:
        messages = self._data.get(conversation_id)
        if messages is None:
            return []
        self._data.move_to_end(conversation_id)
        return list(messages)

    async def append(self, conversation_id: str, messages: Sequence[ChatMessage]) -> None:
        stored = self._data.setdefault(conversation_id, [])
        stored.extend(messages)
        del stored[: max(0, len(stored) - self._max_messages)]
        self._data.move_to_end(conversation_id)
        while len(self._data) > self._max_conversations:
            self._data.popitem(last=False)
