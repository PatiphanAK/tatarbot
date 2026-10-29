"""`VectorStore` adapter in process memory: exact cosine search over every chunk.

Vectors are normalised once when stored, so a search is one dot product per chunk, in a
worker thread so the event loop keeps serving. Pure Python: about 25 ms for 500 chunks and
100 ms for 2,000 at 1,024 dimensions. Fine for a hand-written knowledge base; beyond that,
numpy, an ANN index or pgvector behind the same port.
"""

import asyncio
import heapq
import math
from collections.abc import Sequence

from agent_runtime.application.ports import VectorStore
from agent_runtime.domain import Chunk, EmbeddingError, RetrievedChunk


class InMemoryVectorStore(VectorStore):
    def __init__(self):
        self._chunks: tuple[Chunk, ...] = ()
        self._vectors: tuple[tuple[float, ...], ...] = ()
        self._dimensions = 0

    async def replace(self, chunks: Sequence[Chunk], vectors: Sequence[list[float]]) -> None:
        if len(chunks) != len(vectors):
            raise ValueError("one vector per chunk")
        dimensions = {len(v) for v in vectors}
        if len(dimensions) > 1:
            raise EmbeddingError("vectors have mixed dimensions")
        normalized = tuple(_normalize(v) for v in vectors)
        # One assignment each, no await in between: searches see the old or the new index.
        self._chunks, self._vectors = tuple(chunks), normalized
        self._dimensions = dimensions.pop() if dimensions else 0

    async def search(self, vector: list[float], top_k: int) -> list[RetrievedChunk]:
        chunks, vectors = self._chunks, self._vectors
        if not chunks or top_k <= 0:
            return []
        if len(vector) != self._dimensions:
            raise EmbeddingError(
                f"query has {len(vector)} dimensions, the index {self._dimensions}"
            )
        best = await asyncio.to_thread(_nearest, _normalize(vector), vectors, top_k)
        return [RetrievedChunk(chunks[i], score) for score, i in best]

    async def count(self) -> int:
        return len(self._chunks)


def _nearest(
    query: tuple[float, ...], vectors: tuple[tuple[float, ...], ...], top_k: int
) -> list[tuple[float, int]]:
    scored = ((sum(a * b for a, b in zip(query, v, strict=True)), i) for i, v in enumerate(vectors))
    return heapq.nlargest(top_k, scored)


def _normalize(vector: Sequence[float]) -> tuple[float, ...]:
    norm = math.sqrt(sum(x * x for x in vector))
    if norm == 0:
        return tuple(vector)
    return tuple(x / norm for x in vector)
