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


@pytest.mark.parametrize(
    ("raw", "expected"),
    [
        ("2026-11-01", "2026-11-01"),
        ("2026-11-01T08:30:00Z", "2026-11-01"),
        ("2026-11-01T23:30:00+07:00", "2026-11-01"),
        ("Sat, 01 Nov 2026 08:00:00 GMT", "2026-11-01"),
        ("yesterday", None),
        ("", None),
        (None, None),
        (20261101, None),
    ],
)
def test_published_date_is_normalised_or_dropped(raw, expected):
    from agent_runtime.adapters.outbound.tavily import published_date

    assert published_date(raw) == expected


async def test_results_carry_the_published_date():
    payload = {
        "results": [
            {
                "title": "A",
                "url": "https://a.example",
                "content": "x",
                "published_date": "2026-11-01",
            },
            {"title": "B", "url": "https://b.example", "content": "y"},
        ]
    }
    search, _ = make_search(lambda r: httpx.Response(200, json=payload))

    a, b = await search.search("gold", 5)

    assert (a.published, b.published) == ("2026-11-01", None)
