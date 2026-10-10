"""Web search results as the agent sees them, independent of the search provider."""

from dataclasses import dataclass


@dataclass(frozen=True, slots=True)
class SearchResult:
    title: str
    url: str
    snippet: str
