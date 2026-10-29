"""`EmbeddingPort` adapter that needs no model: hashed character n-grams.

Each text becomes a bag of character trigrams (after lowercasing and collapsing
whitespace), hashed into `dimensions` buckets with a random sign, then L2-normalised. It
matches shared wording, not meaning: "upload a song" finds "/upload", but not "add music".
Character n-grams work for Thai, which has no spaces between words.

Use it for development, tests, or a small knowledge base when no embedding server is
available. Similarity scores run lower than a neural model's; tune `RAG_MIN_SCORE`.
"""

import hashlib
import math
from collections.abc import Sequence

from agent_runtime.application.ports import EmbeddingPort

NGRAM = 3


class HashEmbeddings(EmbeddingPort):
    def __init__(self, dimensions: int = 512):
        if dimensions < 16:
            raise ValueError("dimensions must be at least 16")
        self._dimensions = dimensions

    @property
    def identity(self) -> str:
        return f"hash:char{NGRAM}:{self._dimensions}"

    async def embed_documents(self, texts: Sequence[str]) -> list[list[float]]:
        return [self._embed(t) for t in texts]

    async def embed_query(self, text: str) -> list[float]:
        return self._embed(text)

    def _embed(self, text: str) -> list[float]:
        vector = [0.0] * self._dimensions
        normalized = f" {' '.join(text.lower().split())} "
        for i in range(max(len(normalized) - NGRAM + 1, 1)):
            digest = hashlib.blake2b(normalized[i : i + NGRAM].encode(), digest_size=8).digest()
            value = int.from_bytes(digest, "big")
            vector[value % self._dimensions] += 1.0 if value >> 63 else -1.0
        norm = math.sqrt(sum(x * x for x in vector)) or 1.0
        return [x / norm for x in vector]
