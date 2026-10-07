import json

import httpx
import pytest

from agent_runtime.adapters.outbound.vllm import VLLMModel, strip_reasoning
from agent_runtime.domain import (
    ChatMessage,
    GenerationParams,
    InferenceRequest,
    ModelRequestRejectedError,
    ModelResponseError,
    ModelTimeoutError,
    ModelUnavailableError,
    Role,
)

pytestmark = pytest.mark.anyio

BASE = "http://vllm.test/v1"
REQUEST = InferenceRequest(
    messages=(ChatMessage(Role.SYSTEM, "sys"), ChatMessage(Role.USER, "Hello")),
    params=GenerationParams(max_tokens=50, temperature=0.5),
)


def completion(content="Hi!", finish="stop"):
    return {
        "choices": [
            {
                "index": 0,
                "message": {"role": "assistant", "content": content},
                "finish_reason": finish,
            }
        ],
        "usage": {"prompt_tokens": 10, "completion_tokens": 3, "total_tokens": 13},
    }


def make_model(handler, **kwargs):
    seen = []

    def record(request: httpx.Request):
        seen.append(request)
        return handler(request)

    client = httpx.AsyncClient(
        base_url=BASE + "/",
        transport=httpx.MockTransport(record),
        headers={"Authorization": "Bearer model-key"},
    )
    return VLLMModel(BASE, "qwen", client=client, **kwargs), seen


async def test_translates_request_to_chat_completions():
    model, seen = make_model(lambda r: httpx.Response(200, json=completion()))

    result = await model.generate(REQUEST)

    [request] = seen
    assert request.method == "POST"
    assert str(request.url) == f"{BASE}/chat/completions"
    assert request.headers["authorization"] == "Bearer model-key"
    assert json.loads(request.content) == {
        "model": "qwen",
        "messages": [
            {"role": "system", "content": "sys"},
            {"role": "user", "content": "Hello"},
        ],
        "stream": False,
        "max_tokens": 50,
        "temperature": 0.5,
        "chat_template_kwargs": {"enable_thinking": False},
    }
    assert result.content == "Hi!"
    assert result.finish_reason == "stop"
    assert result.usage.completion_tokens == 3


async def test_omits_optional_fields():
    model, seen = make_model(lambda r: httpx.Response(200, json=completion()), enable_thinking=None)
    await model.generate(InferenceRequest(messages=REQUEST.messages))

    body = json.loads(seen[0].content)
    assert set(body) == {"model", "messages", "stream"}


async def test_strips_reasoning_blocks():
    content = "<think>\nlet me think\n</think>\n\nHello there"
    model, _ = make_model(lambda r: httpx.Response(200, json=completion(content)))
    assert (await model.generate(REQUEST)).content == "Hello there"


@pytest.mark.parametrize(
    ("raw", "expected"),
    [
        ("plain", "plain"),
        ("<think>a</think>b<think>c</think>d", "bd"),
        ("answer<think>truncated reasoning", "answer"),
    ],
)
def test_strip_reasoning(raw, expected):
    assert strip_reasoning(raw) == expected


@pytest.mark.parametrize(
    "exc",
    [httpx.ReadTimeout("slow"), httpx.ConnectTimeout("slow"), httpx.PoolTimeout("slow")],
)
async def test_timeout(exc):
    def handler(request):
        raise exc

    model, _ = make_model(handler)
    with pytest.raises(ModelTimeoutError):
        await model.generate(REQUEST)


async def test_connection_error_is_unavailable():
    def handler(request):
        raise httpx.ConnectError("refused")

    model, _ = make_model(handler)
    with pytest.raises(ModelUnavailableError):
        await model.generate(REQUEST)


@pytest.mark.parametrize(
    ("status", "error"),
    [
        (500, ModelUnavailableError),
        (502, ModelUnavailableError),
        (503, ModelUnavailableError),
        (429, ModelUnavailableError),
        (504, ModelTimeoutError),
        (408, ModelTimeoutError),
        (400, ModelRequestRejectedError),
        (401, ModelRequestRejectedError),
        (404, ModelRequestRejectedError),
    ],
)
async def test_http_errors(status, error):
    model, _ = make_model(lambda r: httpx.Response(status, json={"error": "boom"}))
    with pytest.raises(error):
        await model.generate(REQUEST)


@pytest.mark.parametrize(
    "response",
    [
        httpx.Response(200, text="<html>not json</html>"),
        httpx.Response(200, json={}),
        httpx.Response(200, json={"choices": []}),
        httpx.Response(200, json={"choices": [{"message": {}}]}),
        httpx.Response(200, json={"choices": [{"message": {"content": None}}]}),
        httpx.Response(200, json=[1, 2]),
    ],
)
async def test_malformed_response(response):
    model, _ = make_model(lambda r: response)
    with pytest.raises(ModelResponseError):
        await model.generate(REQUEST)


@pytest.mark.parametrize(
    ("handler", "ready"),
    [
        (lambda r: httpx.Response(200, json={"data": [{"id": "qwen"}]}), True),
        (lambda r: httpx.Response(200, json={"data": [{"id": "other"}]}), False),
        (lambda r: httpx.Response(503), False),
        (lambda r: httpx.Response(200, text="nope"), False),
    ],
)
async def test_is_ready(handler, ready):
    model, seen = make_model(handler)
    assert await model.is_ready() is ready
    assert str(seen[0].url) == f"{BASE}/models"


async def test_is_ready_false_when_unreachable():
    def handler(request):
        raise httpx.ConnectError("refused")

    model, _ = make_model(handler)
    assert await model.is_ready() is False
