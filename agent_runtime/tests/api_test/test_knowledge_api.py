import dataclasses
import time

import pytest
from conftest import API_KEY, FakeModel
from fastapi.testclient import TestClient

from agent_runtime.adapters.outbound.hash_embeddings import HashEmbeddings
from agent_runtime.config import ConfigError, Settings
from agent_runtime.domain import EmbeddingError
from agent_runtime.main import build_embedder, create_app

AUTH = {"Authorization": f"Bearer {API_KEY}"}


def wait_for_index(client, timeout=5.0):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        body = client.get("/v1/knowledge", headers=AUTH).json()
        if body["indexed_at"]:
            return body
        time.sleep(0.02)
    raise AssertionError("knowledge base was not indexed")


@pytest.fixture
def docs(tmp_path):
    d = tmp_path / "knowledge"
    d.mkdir()
    (d / "faq.md").write_text("# FAQ\n\n## Uploads\n\nUploads are limited to 25 MB per file.\n")
    return d


@pytest.fixture
def rag_settings(settings, docs, tmp_path):
    return dataclasses.replace(
        settings,
        rag_docs_dir=str(docs),
        rag_cache_file=str(tmp_path / "cache.json"),
        rag_min_score=0.1,
        embedding_provider="hash",
    )


def test_index_on_startup_and_retrieve_in_chat(rag_settings):
    model = FakeModel(replies=["25 MB."])
    with TestClient(create_app(rag_settings, model=model)) as client:
        status = wait_for_index(client)
        assert (status["documents"], status["chunks"]) == (1, 1)
        assert status["embedder"] == "hash:char3:512"

        response = client.post(
            "/v1/chat", json={"message": "upload size limit?", "conversation_id": "c"}, headers=AUTH
        )
        assert response.status_code == 200
        assert "limited to 25 MB" in model.requests[0].messages[0].content


def test_reload_picks_up_new_documents(rag_settings, docs):
    with TestClient(create_app(rag_settings, model=FakeModel())) as client:
        wait_for_index(client)
        (docs / "rules.md").write_text("# Rules\n\nNo spam.")
        response = client.post("/v1/knowledge/reload", headers=AUTH)
        assert response.status_code == 200
        assert response.json()["documents"] == 2


def test_knowledge_endpoints_need_the_key(rag_settings):
    with TestClient(create_app(rag_settings, model=FakeModel())) as client:
        assert client.get("/v1/knowledge").status_code == 401
        assert client.post("/v1/knowledge/reload").status_code == 401


def test_knowledge_endpoints_404_when_disabled(settings):
    with TestClient(create_app(settings, model=FakeModel())) as client:
        response = client.get("/v1/knowledge", headers=AUTH)
        assert response.status_code == 404
        assert response.json() == {"error": "knowledge base disabled"}


class BrokenEmbeddings(HashEmbeddings):
    async def embed_documents(self, texts):
        raise EmbeddingError("backend down at http://secret.internal")


def test_failed_indexing_keeps_serving_and_reload_maps_to_503(rag_settings):
    settings = dataclasses.replace(rag_settings, rag_cache_file=None)
    model = FakeModel(replies=["ok"])
    with TestClient(create_app(settings, model=model, embedder=BrokenEmbeddings())) as client:
        response = client.post(
            "/v1/chat", json={"message": "hi", "conversation_id": "c"}, headers=AUTH
        )
        assert response.status_code == 200

        response = client.post("/v1/knowledge/reload", headers=AUTH)
        assert response.status_code == 503
        assert response.json() == {"error": "embedding unavailable"}


# ---- configuration


def env(monkeypatch, **values):
    base = {"AGENT_API_KEY": "k", "MODEL_BASE_URL": "http://model/v1", "MODEL_NAME": "m"}
    for key, value in {**base, **values}.items():
        monkeypatch.setenv(key, value)


def test_rag_is_off_by_default(monkeypatch):
    env(monkeypatch)
    assert Settings.from_env().rag_docs_dir is None


def test_embedding_backend_defaults_to_the_model_server(monkeypatch):
    env(monkeypatch, RAG_DOCS_DIR="/k", EMBEDDING_MODEL="bge-m3", MODEL_API_KEY="model-key")
    s = Settings.from_env()
    assert (s.embedding_base_url, s.embedding_api_key) == ("http://model/v1", "model-key")
    assert build_embedder(s).identity.startswith("openai:bge-m3:native:")


def test_model_key_is_not_sent_to_another_embedding_host(monkeypatch):
    env(
        monkeypatch,
        RAG_DOCS_DIR="/k",
        EMBEDDING_MODEL="e",
        EMBEDDING_BASE_URL="http://ollama:11434/v1",
        MODEL_API_KEY="model-key",
    )
    assert Settings.from_env().embedding_api_key is None


def test_prefixes_keep_whitespace_and_newlines(monkeypatch):
    env(
        monkeypatch,
        RAG_DOCS_DIR="/k",
        EMBEDDING_MODEL="e",
        EMBEDDING_QUERY_PREFIX="Instruct: find docs\\nQuery: ",
        EMBEDDING_DOCUMENT_PREFIX="passage: ",
    )
    s = Settings.from_env()
    assert s.embedding_query_prefix == "Instruct: find docs\nQuery: "
    assert s.embedding_document_prefix == "passage: "


@pytest.mark.parametrize(
    "values",
    [
        {"RAG_DOCS_DIR": "/k"},  # openai provider without a model
        {"RAG_DOCS_DIR": "/k", "EMBEDDING_PROVIDER": "word2vec"},
        {"RAG_DOCS_DIR": "/k", "EMBEDDING_PROVIDER": "hash", "RAG_CHUNK_OVERLAP": "900"},
    ],
)
def test_invalid_rag_config(monkeypatch, values):
    env(monkeypatch, **values)
    with pytest.raises(ConfigError):
        Settings.from_env()


def test_hash_provider_needs_no_model(monkeypatch):
    env(monkeypatch, RAG_DOCS_DIR="/k", EMBEDDING_PROVIDER="HASH", EMBEDDING_DIMENSIONS="128")
    assert build_embedder(Settings.from_env()).identity == "hash:char3:128"
