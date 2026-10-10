"""`SearchPort` adapter for the Tavily Search API (https://docs.tavily.com)."""

import logging

import httpx

from agent_runtime.application.ports import SearchPort
from agent_runtime.domain import SearchError, SearchResult

log = logging.getLogger(__name__)

TAVILY_URL = "https://api.tavily.com/"
# Each result's text is cut to this length before it reaches the model.
MAX_SNIPPET_CHARS = 800


class TavilySearch(SearchPort):
    def __init__(
        self,
        api_key: str,
        *,
        timeout_seconds: float = 10.0,
        search_depth: str = "basic",
        client: httpx.AsyncClient | None = None,
    ):
        self._client = client or httpx.AsyncClient(
            base_url=TAVILY_URL,
            headers={"Authorization": f"Bearer {api_key}"},
            timeout=timeout_seconds,
        )
        self._search_depth = search_depth

    async def search(self, query: str, max_results: int) -> list[SearchResult]:
        body = {"query": query, "max_results": max_results, "search_depth": self._search_depth}
        try:
            response = await self._client.post("search", json=body)
        except httpx.HTTPError as e:
            raise SearchError(f"search unreachable: {type(e).__name__}") from e
        if response.status_code != 200:
            # Never log the request headers (API key); a body prefix is enough.
            log.warning("search returned HTTP %s: %.300s", response.status_code, response.text)
            raise SearchError(f"search failed (HTTP {response.status_code})")
        try:
            results = response.json()["results"]
            return [
                SearchResult(
                    title=str(r.get("title") or ""),
                    url=str(r["url"]),
                    snippet=" ".join(str(r.get("content") or "").split())[:MAX_SNIPPET_CHARS],
                )
                for r in results
            ][:max_results]
        except (ValueError, KeyError, TypeError, AttributeError) as e:
            raise SearchError("malformed search response") from e

    async def aclose(self) -> None:
        await self._client.aclose()
