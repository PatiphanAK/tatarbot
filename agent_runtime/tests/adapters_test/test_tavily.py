import json

import httpx
import pytest

from agent_runtime.adapters.outbound.tavily import MAX_SNIPPET_CHARS, TavilySearch
from agent_runtime.domain import SearchError, SearchResult

pytestmark = pytest.mark.anyio


def make_search(handler):
    seen = []

    def record(request: httpx.Request):
        seen.append(request)
        return handler(request)

    client = httpx.AsyncClient(
        base_url="https://tavily.test/",
        transport=httpx.MockTransport(record),
        headers={"Authorization": "Bearer tvly-key"},
    )
    return TavilySearch("tvly-key", client=client), seen


async def test_search_request_and_results():
    payload = {
        "results": [
            {"title": "A", "url": "https://a.example", "content": "first  \n result"},
            {"title": None, "url": "https://b.example", "content": "x" * 2000},
        ]
    }
    search, seen = make_search(lambda r: httpx.Response(200, json=payload))

    results = await search.search("bangkok weather", 5)

    [request] = seen
    assert str(request.url) == "https://tavily.test/search"
    assert request.headers["authorization"] == "Bearer tvly-key"
    assert json.loads(request.content) == {
        "query": "bangkok weather",
        "max_results": 5,
        "search_depth": "basic",
    }
    assert results[0] == SearchResult("A", "https://a.example", "first result")
    assert results[1].title == ""
    assert len(results[1].snippet) == MAX_SNIPPET_CHARS


@pytest.mark.parametrize(
    "handler",
    [
        lambda r: httpx.Response(401, json={"detail": "bad key"}),
        lambda r: httpx.Response(429, json={}),
        lambda r: httpx.Response(200, json={"unexpected": True}),
        lambda r: httpx.Response(200, text="not json"),
        lambda r: (_ for _ in ()).throw(httpx.ConnectError("down")),
    ],
)
async def test_failures_raise_search_error(handler):
    search, _ = make_search(handler)
    with pytest.raises(SearchError):
        await search.search("q", 3)
