"""The Sertis smoke-test turn ("ราคาทองคำวันนี้เท่าไร", 3 searches) replayed through the real
HTTP model adapter against a mocked OpenAI Chat Completions API. No network.

Before planned searches this turn made 8 model calls; now 5."""

import json

import httpx
import pytest

from agent_runtime.adapters.outbound.memory_store import InMemoryConversationStore
from agent_runtime.adapters.outbound.vllm import VLLMModel
from agent_runtime.application import ChatService, ChatSettings, SearchPort
from agent_runtime.domain import SearchResult

pytestmark = pytest.mark.anyio

# `Next:` on its own line, as gpt-4.1-mini wrote it in most live steps.
THOUGHTS = [
    "The user wants today's gold price.\nNext: SEARCH ราคาทองคำวันนี้",
    "Results are from yesterday.\nNext: SEARCH ราคาทองคำ 5 พฤศจิกายน 2569",
    "Still not sure.\nNext: SEARCH ราคาทองคำแท่ง ล่าสุด",
    "The results are enough.\nNext: ANSWER",
]
ANSWER = "ราคาทองคำแท่งวันนี้ รับซื้อ 62,650 บาท ขายออก 62,850 บาท"


class Gold(SearchPort):
    def __init__(self):
        self.queries = []

    async def search(self, query, max_results):
        self.queries.append(query)
        return [SearchResult("ราคาทอง", "https://gold.example/today", "62,650 / 62,850")]


def openai_api():
    """Replies like OpenAI: a thought per reasoning call, then the answer, with usage."""
    return openai_api_with([*THOUGHTS, ANSWER])


def openai_api_with(script):
    script = list(script)
    bodies = []

    def handle(request: httpx.Request) -> httpx.Response:
        body = json.loads(request.content)
        bodies.append(body)
        if "chat_template_kwargs" in body:  # OpenAI rejects unknown fields
            return httpx.Response(400, json={"error": {"message": "Unrecognized argument"}})
        return httpx.Response(
            200,
            json={
                "choices": [
                    {
                        "message": {"role": "assistant", "content": script.pop(0)},
                        "finish_reason": "stop",
                    }
                ],
                "usage": {"prompt_tokens": 1000, "completion_tokens": 50},
            },
        )

    return handle, bodies


def chat(handler, enable_thinking=None):
    client = httpx.AsyncClient(
        base_url="https://api.openai.test/v1/", transport=httpx.MockTransport(handler)
    )
    model = VLLMModel(
        "https://api.openai.test/v1", "gpt-4.1-mini", client=client, enable_thinking=enable_thinking
    )
    search = Gold()
    settings = ChatSettings(
        system_prompt="You are TatarBot.", price_input_per_mtok=0.4, price_output_per_mtok=1.6
    )
    return ChatService(model, InMemoryConversationStore(), settings, search=search), search


async def test_gold_turn_makes_five_model_calls_with_openai_shaped_requests(caplog):
    handler, bodies = openai_api()
    service, search = chat(handler)

    with caplog.at_level("INFO"):
        reply = await service.chat("discord:1:2:3", "ราคาทองคำวันนี้เท่าไร")

    assert reply.startswith(ANSWER)
    assert search.queries == [
        "ราคาทองคำวันนี้",
        "ราคาทองคำ 5 พฤศจิกายน 2569",
        "ราคาทองคำแท่ง ล่าสุด",
    ]
    assert len(bodies) == 5  # 4 thoughts + the answer; was 8
    assert all("chat_template_kwargs" not in b for b in bodies)
    assert all("tools" not in b for b in bodies)  # no repeat-the-query tool calls

    # The last request carries the 3 planned calls, each answered by its tool message.
    messages = bodies[-1]["messages"]
    calls = [c for m in messages if m["role"] == "assistant" for c in m.get("tool_calls", [])]
    answered = [m["tool_call_id"] for m in messages if m["role"] == "tool"]
    assert [c["id"] for c in calls] == answered and len(set(answered)) == 3
    assert all(c["type"] == "function" and c["function"]["name"] == "web_search" for c in calls)
    assert json.loads(calls[0]["function"]["arguments"]) == {"query": "ราคาทองคำวันนี้"}

    line = next(r.getMessage() for r in caplog.records if "chat turn completed" in r.message)
    assert "searches=3 actions=0 model_calls=5 input_tokens=5000 output_tokens=250" in line
    assert "cost_usd=0.002400" in line


async def test_qwen_thinking_flag_would_be_rejected_by_openai():
    """Why MODEL_ENABLE_THINKING=omit is needed for OpenAI: false still sends the field."""
    handler, bodies = openai_api()
    service, _ = chat(handler, enable_thinking=False)

    with pytest.raises(Exception, match="rejected"):
        await service.chat("c", "hi")

    assert len(bodies) == 1  # rejected once, not retried


async def test_decision_at_the_end_of_a_sentence_is_planned_not_unclear():
    """The live smoke test (2026-11-05) had one `unclear` step: gpt-4.1-mini wrote the
    decision at the end of its thought. With the parser fix (PR #25) it is a planned search:
    no extra model call."""
    handler, bodies = openai_api_with(
        [
            "Gold prices change daily, I need today's. Next: SEARCH ราคาทองคำวันนี้",
            "The result answers it. Next: ANSWER",
            ANSWER,
        ]
    )
    service, search = chat(handler)

    await service.chat("c", "ราคาทองคำวันนี้เท่าไร")

    assert search.queries == ["ราคาทองคำวันนี้"]
    assert len(bodies) == 3  # thought, thought, answer
    assert all("tools" not in b for b in bodies)
