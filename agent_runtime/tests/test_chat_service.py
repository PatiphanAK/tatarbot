import asyncio

import pytest
from conftest import SYSTEM_PROMPT, FakeModel

from agent_runtime.adapters.outbound.memory_store import InMemoryConversationStore
from agent_runtime.application import ChatService, ChatSettings
from agent_runtime.domain import (
    ChatMessage,
    GenerationParams,
    InvalidInputError,
    ModelResponseError,
    Role,
)

pytestmark = pytest.mark.anyio


def service(model, max_history=20, store=None):
    return ChatService(
        model,
        store or InMemoryConversationStore(),
        ChatSettings(
            system_prompt=SYSTEM_PROMPT,
            max_history_messages=max_history,
            generation=GenerationParams(max_tokens=64, temperature=0.2),
        ),
    )


async def test_model_port_receives_system_history_and_params():
    model = FakeModel(replies=["a1", "a2"])
    chat = service(model)

    assert await chat.chat("c", "  q1  ") == "a1"
    assert await chat.chat("c", "q2") == "a2"

    request = model.requests[1]
    assert request.messages == (
        ChatMessage(Role.SYSTEM, SYSTEM_PROMPT),
        ChatMessage(Role.USER, "q1"),
        ChatMessage(Role.ASSISTANT, "a1"),
        ChatMessage(Role.USER, "q2"),
    )
    assert request.params == GenerationParams(max_tokens=64, temperature=0.2)


async def test_history_window_starts_with_user_message():
    model = FakeModel()
    chat = service(model, max_history=3)
    for i in range(3):
        await chat.chat("c", f"q{i}")

    roles = [m.role for m in model.requests[-1].messages]
    # Window of 3 would start on an assistant message; it is dropped.
    assert roles == [Role.SYSTEM, Role.USER, Role.ASSISTANT, Role.USER]


async def test_zero_history_sends_only_system_and_user():
    model = FakeModel()
    chat = service(model, max_history=0)
    await chat.chat("c", "q0")
    await chat.chat("c", "q1")

    assert [m.content for m in model.requests[-1].messages] == [SYSTEM_PROMPT, "q1"]


async def test_empty_model_reply_is_an_error_and_not_stored():
    store = InMemoryConversationStore()
    chat = service(FakeModel(replies=["   "]), store=store)

    with pytest.raises(ModelResponseError):
        await chat.chat("c", "q")
    assert await store.load("c") == []


@pytest.mark.parametrize(("cid", "msg"), [("c", "  "), ("", "hi"), ("x" * 257, "hi")])
async def test_rejects_invalid_input(cid, msg):
    model = FakeModel()
    with pytest.raises(InvalidInputError):
        await service(model).chat(cid, msg)
    assert model.requests == []


async def test_concurrent_turns_in_one_conversation_are_serialised():
    class SlowModel(FakeModel):
        async def generate(self, request):
            await asyncio.sleep(0.01)
            return await super().generate(request)

    model = SlowModel()
    chat = service(model)
    await asyncio.gather(chat.chat("c", "q1"), chat.chat("c", "q2"))

    # The second request must see the first exchange in its history.
    assert len(model.requests[1].messages) == 4
