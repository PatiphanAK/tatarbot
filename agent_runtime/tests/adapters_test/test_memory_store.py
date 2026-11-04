import pytest

from agent_runtime.adapters.outbound.memory_store import InMemoryConversationStore
from agent_runtime.domain import ChatMessage, Role

pytestmark = pytest.mark.anyio


def msg(text):
    return ChatMessage(Role.USER, text)


async def test_unknown_conversation_is_empty():
    assert await InMemoryConversationStore().load("nope") == []


async def test_trims_oldest_messages():
    store = InMemoryConversationStore(max_messages_per_conversation=3)
    await store.append("c", [msg("1"), msg("2")])
    await store.append("c", [msg("3"), msg("4")])
    assert [m.content for m in await store.load("c")] == ["2", "3", "4"]


async def test_evicts_least_recently_used_conversation():
    store = InMemoryConversationStore(max_conversations=2)
    await store.append("a", [msg("a")])
    await store.append("b", [msg("b")])
    await store.load("a")
    await store.append("c", [msg("c")])
    assert await store.load("b") == []
    assert await store.load("a") != []


async def test_load_returns_a_copy():
    store = InMemoryConversationStore()
    await store.append("c", [msg("1")])
    (await store.load("c")).append(msg("x"))
    assert len(await store.load("c")) == 1
