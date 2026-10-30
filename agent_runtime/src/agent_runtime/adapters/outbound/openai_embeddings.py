"""`EmbeddingPort` adapter for the OpenAI-compatible Embeddings API (`POST /embeddings`).

Works with any server that speaks it: vLLM (`--task embed`), Ollama, Text Embeddings
Inference, LiteLLM, OpenAI, Alibaba Model Studio's compatible mode... The model is chosen
by configuration; nothing here is specific to one.

Many embedding models expect different input for queries and documents, e.g. E5
(`query: ` / `passage: `) or Qwen3-Embedding (`Instruct: <task>\\nQuery: ` for queries only).
Those prefixes are configuration too, and part of `identity`.
"""

import hashlib
import logging
from collections.abc import Sequence

import httpx

from agent_runtime.application.ports import EmbeddingPort
from agent_runtime.domain import EmbeddingError

log = logging.getLogger(__name__)


class OpenAIEmbeddings(EmbeddingPort):
    def __init__(
        self,
        base_url: str,
        model_name: str,
        *,
        api_key: str | None = None,
        dimensions: int | None = None,
        query_prefix: str = "",
        document_prefix: str = "",
        batch_size: int = 32,
        timeout_seconds: float = 30.0,
        client: httpx.AsyncClient | None = None,
    ):
        """`base_url` is the API root, e.g. `https://host/v1`. `dimensions` is sent only
        when set (models with Matryoshka embeddings can return shorter vectors)."""
        if batch_size < 1:
            raise ValueError("batch_size must be at least 1")
        headers = {"Authorization": f"Bearer {api_key}"} if api_key else {}
        self._client = client or httpx.AsyncClient(
            base_url=base_url.rstrip("/") + "/",
            headers=headers,
            timeout=timeout_seconds,
        )
        self._model_name = model_name
        self._dimensions = dimensions
        self._query_prefix = query_prefix
        self._document_prefix = document_prefix
        self._batch_size = batch_size

    @property
    def identity(self) -> str:
        prefixes = hashlib.sha256(
            f"{self._query_prefix}\0{self._document_prefix}".encode()
        ).hexdigest()[:8]
        return f"openai:{self._model_name}:{self._dimensions or 'native'}:{prefixes}"

    async def embed_documents(self, texts: Sequence[str]) -> list[list[float]]:
        vectors: list[list[float]] = []
        for start in range(0, len(texts), self._batch_size):
            batch = texts[start : start + self._batch_size]
            vectors.extend(await self._embed([self._document_prefix + t for t in batch]))
        return vectors

    async def embed_query(self, text: str) -> list[float]:
        [vector] = await self._embed([self._query_prefix + text])
        return vector

    async def aclose(self) -> None:
        await self._client.aclose()

    async def _embed(self, inputs: list[str]) -> list[list[float]]:
        body: dict = {"model": self._model_name, "input": inputs, "encoding_format": "float"}
        if self._dimensions:
            body["dimensions"] = self._dimensions
        try:
            response = await self._client.post("embeddings", json=body)
        except httpx.TimeoutException as e:
            raise EmbeddingError("embedding request timed out") from e
        except httpx.TransportError as e:
            raise EmbeddingError(f"embedding backend unreachable: {type(e).__name__}") from e
        if response.status_code != 200:
            # Never log the request headers (API key); a body prefix is enough.
            log.warning("embeddings returned HTTP %s: %.300s", response.status_code, response.text)
            raise EmbeddingError(f"embedding request failed (HTTP {response.status_code})")
        try:
            data = sorted(response.json()["data"], key=lambda d: d["index"])
            vectors = [[float(x) for x in d["embedding"]] for d in data]
        except (ValueError, KeyError, TypeError) as e:
            raise EmbeddingError("malformed embedding response") from e
        if len(vectors) != len(inputs) or not all(vectors):
            raise EmbeddingError("embedding response does not match the input")
        if len({len(v) for v in vectors}) != 1:
            raise EmbeddingError("embedding response has mixed dimensions")
        return vectors
