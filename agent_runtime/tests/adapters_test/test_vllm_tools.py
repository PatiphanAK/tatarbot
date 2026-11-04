import json

import httpx
import pytest
from test_vllm import make_model

from agent_runtime.domain import (
    ChatMessage,
    InferenceRequest,
    ModelResponseError,
    Role,
    ToolCall,
    ToolSpec,
)

pytestmark = pytest.mark.anyio

TOOL = ToolSpec("web_search", "Search the web.", {"type": "object", "properties": {}})
CALL = ToolCall("call_1", "web_search", '{"query": "bangkok weather"}')


def tool_call_completion(content=None):
    return {
        "choices": [
            {
                "message": {
                    "role": "assistant",
                    "content": content,
                    "tool_calls": [
                        {
                            "id": "call_1",
                            "type": "function",
                            "function": {
                                "name": "web_search",
                                "arguments": '{"query": "bangkok weather"}',
                            },
                        }
                    ],
                },
                "finish_reason": "tool_calls",
            }
        ]
    }


async def test_sends_tools_and_tool_messages():
    model, seen = make_model(lambda r: httpx.Response(200, json=tool_call_completion()))
    request = InferenceRequest(
        messages=(
            ChatMessage(Role.USER, "weather?"),
            ChatMessage(Role.ASSISTANT, "", tool_calls=(CALL,)),
            ChatMessage(Role.TOOL, "sunny", tool_call_id="call_1"),
        ),
        tools=(TOOL,),
    )

    await model.generate(request)

    body = json.loads(seen[0].content)
    assert body["tools"] == [
        {
            "type": "function",
            "function": {
                "name": "web_search",
                "description": "Search the web.",
                "parameters": {"type": "object", "properties": {}},
            },
        }
    ]
    assert body["tool_choice"] == "auto"
    assert body["messages"][1:] == [
        {
            "role": "assistant",
            "content": "",
            "tool_calls": [
                {
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "web_search", "arguments": CALL.arguments},
                }
            ],
        },
        {"role": "tool", "content": "sunny", "tool_call_id": "call_1"},
    ]


async def test_no_tools_field_without_tools():
    model, seen = make_model(lambda r: httpx.Response(200, json=tool_call_completion("x")))
    await model.generate(InferenceRequest(messages=(ChatMessage(Role.USER, "hi"),)))

    body = json.loads(seen[0].content)
    assert "tools" not in body
    assert "tool_choice" not in body


@pytest.mark.parametrize("content", [None, "<think>plan</think>"])
async def test_parses_tool_calls_with_null_or_reasoning_content(content):
    model, _ = make_model(lambda r: httpx.Response(200, json=tool_call_completion(content)))

    result = await model.generate(InferenceRequest(messages=(ChatMessage(Role.USER, "hi"),)))

    assert result.content == ""
    assert result.tool_calls == (CALL,)
    assert result.finish_reason == "tool_calls"


async def test_null_content_without_tool_calls_is_an_error():
    body = {"choices": [{"message": {"role": "assistant", "content": None}}]}
    model, _ = make_model(lambda r: httpx.Response(200, json=body))

    with pytest.raises(ModelResponseError):
        await model.generate(InferenceRequest(messages=(ChatMessage(Role.USER, "hi"),)))


async def test_malformed_tool_call_is_an_error():
    body = tool_call_completion()
    del body["choices"][0]["message"]["tool_calls"][0]["function"]
    model, _ = make_model(lambda r: httpx.Response(200, json=body))

    with pytest.raises(ModelResponseError):
        await model.generate(InferenceRequest(messages=(ChatMessage(Role.USER, "hi"),)))
