import json
import math

import httpx
import pytest

from agent_runtime.adapters.outbound.embedding_cache import CachedEmbeddings
from agent_runtime.adapters.outbound.hash_embeddings import HashEmbeddings
from agent_runtime.adapters.outbound.memory_vectors import InMemoryVectorStore
from agent_runtime.adapters.outbound.openai_embeddings import OpenAIEmbeddings
from agent_runtime.domain import Chunk, EmbeddingError

pytestmark = pytest.mark.anyio


def make_embedder(handler, **kwargs):
    seen = []

    def record(request: httpx.Request):
        seen.append(json.loads(request.content))
        return handler(request)

    client = httpx.AsyncClient(
        base_url="https://emb.test/v1/", transport=httpx.MockTransport(record)
    )
    return OpenAIEmbeddings("unused", "bge-m3", client=client, **kwargs), seen


def echo_lengths(request):
    body = json.loads(request.content)
    # Out of order on purpose: the adapter sorts by index.
    data = [{"index": i, "embedding": [float(len(t)), 1.0]} for i, t in enumerate(body["input"])]
    return httpx.Response(200, json={"data": list(reversed(data))})


# ---- OpenAI-compatible


async def test_documents_are_batched_prefixed_and_ordered():
    embedder, seen = make_embedder(
        echo_lengths, batch_size=2, document_prefix="passage: ", dimensions=256
    )
    vectors = await embedder.embed_documents(["a", "bb", "ccc"])

    assert [b["input"] for b in seen] == [["passage: a", "passage: bb"], ["passage: ccc"]]
    assert all(b["model"] == "bge-m3" and b["dimensions"] == 256 for b in seen)
    assert [v[0] for v in vectors] == [10.0, 11.0, 12.0]


async def test_query_prefix_and_no_dimensions_by_default():
    embedder, seen = make_embedder(echo_lengths, query_prefix="Instruct: find\nQuery: ")
    assert await embedder.embed_query("hi") == [float(len("Instruct: find\nQuery: hi")), 1.0]
    assert "dimensions" not in seen[0]


def test_identity_changes_with_model_settings():
    def identity(**kwargs):
        return OpenAIEmbeddings("http://x/v1", kwargs.pop("model", "m"), **kwargs).identity

    assert identity() == identity()
    assert identity() != identity(model="other")
    assert identity() != identity(dimensions=512)
    assert identity() != identity(query_prefix="query: ")


@pytest.mark.parametrize(
    "response",
    [
        httpx.Response(500, text="boom"),
        httpx.Response(200, json={"nope": []}),
        httpx.Response(200, json={"data": []}),
        httpx.Response(200, json={"data": [{"index": 0, "embedding": []}]}),
    ],
)
async def test_bad_responses_raise_embedding_error(response):
    embedder, _ = make_embedder(lambda r: response)
    with pytest.raises(EmbeddingError):
        await embedder.embed_query("x")


async def test_transport_errors_raise_embedding_error():
    def fail(request):
        raise httpx.ConnectError("refused")

    embedder, _ = make_embedder(fail)
    with pytest.raises(EmbeddingError, match="unreachable"):
        await embedder.embed_query("x")


# ---- hash


async def test_hash_embeddings_are_normalized_and_lexical():
    embedder = HashEmbeddings(256)
    [a, b, c] = await embedder.embed_documents(
        ["upload an audio file", "how to upload audio", "เปิดเพลงแจ๊ส"]
    )
    assert len(a) == 256
    assert math.isclose(sum(x * x for x in a), 1.0)
    dot = lambda u, v: sum(x * y for x, y in zip(u, v, strict=True))  # noqa: E731
    assert dot(a, b) > dot(a, c)
    assert await embedder.embed_query("upload an audio file") == a


# ---- cache


class CountingEmbedder(HashEmbeddings):
    def __init__(self, dimensions=32):
        super().__init__(dimensions)
        self.embedded: list[str] = []

    async def embed_documents(self, texts):
        self.embedded.extend(texts)
        return await super().embed_documents(texts)


async def test_cache_embeds_only_new_texts_and_prunes(tmp_path):
    path = tmp_path / "cache" / "emb.json"
    inner = CountingEmbedder()
    cached = CachedEmbeddings(inner, path)

    first = await cached.embed_documents(["a", "b", "a"])
    assert inner.embedded == ["a", "b"]
    assert first[0] == first[2]

    inner.embedded.clear()
    again = await CachedEmbeddings(inner, path).embed_documents(["b", "c"])
    assert inner.embedded == ["c"]
    assert again[0] == first[1]

    stored = json.loads(path.read_text())
    assert stored["identity"] == inner.identity
    assert len(stored["vectors"]) == 2  # "a" is gone


async def test_cache_from_another_embedder_is_ignored(tmp_path):
    path = tmp_path / "emb.json"
    await CachedEmbeddings(CountingEmbedder(32), path).embed_documents(["a"])

    other = CountingEmbedder(64)
    [vector] = await CachedEmbeddings(other, path).embed_documents(["a"])
    assert other.embedded == ["a"]
    assert len(vector) == 64


async def test_corrupt_cache_is_ignored(tmp_path):
    path = tmp_path / "emb.json"
    path.write_text("{not json")
    inner = CountingEmbedder()
    await CachedEmbeddings(inner, path).embed_documents(["a"])
    assert inner.embedded == ["a"]


# ---- vector store


def chunk(text):
    return Chunk("d", "T", "", text, 0)


async def test_vector_store_cosine_top_k():
    store = InMemoryVectorStore()
    await store.replace([chunk("x"), chunk("y"), chunk("xy")], [[1, 0], [0, 5], [1, 1]])

    hits = await store.search([2, 0], top_k=2)
    assert [h.chunk.text for h in hits] == ["x", "xy"]
    assert math.isclose(hits[0].score, 1.0)
    assert math.isclose(hits[1].score, 1 / math.sqrt(2))
    assert await store.count() == 3


async def test_vector_store_rejects_dimension_mismatch():
    store = InMemoryVectorStore()
    await store.replace([chunk("x")], [[1, 0]])
    with pytest.raises(EmbeddingError):
        await store.search([1, 0, 0], top_k=1)
    with pytest.raises(EmbeddingError):
        await store.replace([chunk("x"), chunk("y")], [[1, 0], [1, 0, 0]])
    assert await store.search([1, 0], top_k=0) == []
