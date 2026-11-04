"""Ingestion pieces that need no database: parsing, namespaces, the SQS worker, the S3
adapter, the upload API, configuration and hybrid-hit filtering."""

import io
import json
from datetime import UTC, datetime

import pytest
from botocore.response import StreamingBody
from botocore.stub import Stubber
from conftest import API_KEY, FakeModel
from fastapi.testclient import TestClient

from agent_runtime.adapters.inbound.sqs_worker import SqsWorker
from agent_runtime.adapters.outbound.s3_storage import S3Storage
from agent_runtime.application import KnowledgeBase, Outcome, Uploads, VectorStore
from agent_runtime.config import ConfigError, Settings
from agent_runtime.domain import (
    Chunk,
    DocumentStatus,
    InvalidDocumentError,
    InvalidInputError,
    ObjectMissingError,
    RetrievedChunk,
    StoredDocument,
)
from agent_runtime.domain.ingestion import (
    PresignedUpload,
    parse_document,
    retrieval_namespaces,
    upload_source,
)
from agent_runtime.main import create_app

# --- parsing and validation


def test_parses_markdown_and_text():
    parsed = parse_document("﻿# Title\r\n\r\nสวัสดี".encode(), ".md", 1000)
    assert parsed.text == "# Title\n\nสวัสดี" and parsed.mime_type == "text/markdown"
    assert parse_document(b"plain", ".txt", 1000).mime_type == "text/plain"


@pytest.mark.parametrize(
    "data, message",
    [
        (b"%PDF-1.7", "PDF"),
        (b"PK\x03\x04rest", "ZIP"),
        (b"\xff\xfe\x00bad", "UTF-8"),
        (b"text\x00with nul", "binary"),
        (b"   \n\n", "no text"),
        (b"", "empty"),
        (b"x" * 101, "larger"),
    ],
)
def test_rejects_content_regardless_of_extension(data, message):
    with pytest.raises(InvalidDocumentError, match=message):
        parse_document(data, ".md", 100)


def test_upload_source_uses_only_the_base_name():
    assert upload_source("..\\..\\x\\notes.MD", None) == ("notes.MD", ".md")
    assert upload_source("a/b/faq.txt", "faq") == ("faq", ".txt")
    with pytest.raises(InvalidDocumentError):
        upload_source("run.sh", None)
    with pytest.raises(InvalidDocumentError):
        upload_source("noextension", None)
    with pytest.raises(InvalidInputError):
        upload_source("a.md", "bad\nsource")


@pytest.mark.parametrize(
    "conversation_id, expected",
    [
        ("discord:123:456:789", ("global", "discord:123")),
        ("discord:dm:456:789", ("global",)),
        ("smoke:test", ("global",)),
        ("discord:123", ("global",)),
    ],
)
def test_retrieval_namespaces(conversation_id, expected):
    assert retrieval_namespaces(conversation_id) == expected


# --- SQS worker


class FakeIngestor:
    def __init__(self, outcome=Outcome.DONE):
        self.calls = []
        self.outcome = outcome

    async def handle_object_created(self, bucket, key, version):
        self.calls.append(("event", bucket, key, version))
        return self.outcome

    async def reprocess(self, document_id, force):
        self.calls.append(("reprocess", document_id, force))
        return self.outcome


class FakeSqs:
    def __init__(self):
        self.deleted = []
        self.visibility = []

    def delete_message(self, QueueUrl, ReceiptHandle):
        self.deleted.append(ReceiptHandle)

    def change_message_visibility(self, QueueUrl, ReceiptHandle, VisibilityTimeout):
        self.visibility.append((ReceiptHandle, VisibilityTimeout))


def s3_event(key, version=None, etag="abc", name="ObjectCreated:Post"):
    obj = {"key": key, "size": 10, "eTag": etag}
    if version:
        obj["versionId"] = version
    return json.dumps(
        {
            "Records": [
                {
                    "eventSource": "aws:s3",
                    "eventName": name,
                    "s3": {"bucket": {"name": "bucket"}, "object": obj},
                }
            ]
        }
    )


@pytest.mark.anyio
async def test_worker_routes_s3_events_and_acks():
    ingestor, sqs = FakeIngestor(), FakeSqs()
    worker = SqsWorker(sqs, "q", ingestor)
    message = {"Body": s3_event("rag/a+b%C3%A9.md"), "ReceiptHandle": "r1"}
    assert await worker.process(message) is Outcome.DONE
    assert ingestor.calls == [("event", "bucket", "rag/a bé.md", "etag:abc")]
    assert sqs.deleted == ["r1"]

    await worker.handle(s3_event("rag/v.md", version="v123"))
    assert ingestor.calls[-1][3] == "v123"


@pytest.mark.anyio
async def test_worker_backs_off_on_retry():
    sqs = FakeSqs()
    worker = SqsWorker(sqs, "q", FakeIngestor(Outcome.RETRY))
    message = {
        "Body": s3_event("rag/a.md"),
        "ReceiptHandle": "r1",
        "Attributes": {"ApproximateReceiveCount": "3"},
    }
    assert await worker.process(message) is Outcome.RETRY
    assert sqs.deleted == [] and sqs.visibility == [("r1", 120)]


@pytest.mark.anyio
async def test_worker_retries_unexpected_errors():
    class Broken(FakeIngestor):
        async def handle_object_created(self, *args):
            raise RuntimeError("database down")

    sqs = FakeSqs()
    assert (
        await SqsWorker(sqs, "q", Broken()).process(
            {"Body": s3_event("rag/a.md"), "ReceiptHandle": "r"}
        )
        is Outcome.RETRY
    )
    assert sqs.deleted == []


@pytest.mark.anyio
@pytest.mark.parametrize(
    "body",
    [
        "not json",
        "[1, 2]",
        json.dumps({"Event": "s3:TestEvent"}),
        json.dumps({"type": "reprocess", "document_id": "x"}),
        json.dumps({"Records": [{"eventSource": "aws:s3", "eventName": "ObjectCreated:Put"}]}),
        s3_event("rag/a.md", name="ObjectRemoved:Delete"),
        json.dumps({"something": "else"}),
    ],
)
async def test_worker_drops_messages_it_cannot_use(body):
    ingestor = FakeIngestor()
    assert await SqsWorker(FakeSqs(), "q", ingestor).handle(body) is Outcome.DONE
    assert ingestor.calls == []


@pytest.mark.anyio
async def test_worker_handles_reprocess_requests():
    ingestor = FakeIngestor()
    body = json.dumps({"type": "reprocess", "document_id": 7, "force": True})
    await SqsWorker(FakeSqs(), "q", ingestor).handle(body)
    assert ingestor.calls == [("reprocess", 7, True)]


# --- S3 adapter


@pytest.fixture
def s3():
    import boto3

    client = boto3.client(
        "s3", region_name="us-east-1", aws_access_key_id="x", aws_secret_access_key="y"
    )
    with Stubber(client) as stub:
        yield S3Storage("bucket", client), stub


def test_presigned_post_enforces_size(s3):
    storage, _ = s3
    upload = storage.presign_upload("rag/abc.md", 1000, 300)
    assert upload.fields["key"] == "rag/abc.md"
    policy = json.loads(__import__("base64").b64decode(upload.fields["policy"]))
    assert ["content-length-range", 1, 1000] in policy["conditions"]


@pytest.mark.anyio
async def test_read_checks_size_before_downloading(s3):
    storage, stub = s3
    stub.add_response(
        "head_object",
        {"ContentLength": 5000},
        {"Bucket": "bucket", "Key": "k", "IfMatch": "abc"},
    )
    with pytest.raises(InvalidDocumentError, match="larger"):
        await storage.read("k", "etag:abc", 1000)


@pytest.mark.anyio
async def test_read_returns_bytes_by_version(s3):
    storage, stub = s3
    params = {"Bucket": "bucket", "Key": "k", "VersionId": "v1"}
    stub.add_response("head_object", {"ContentLength": 5}, params)
    body = StreamingBody(io.BytesIO(b"hello"), 5)
    stub.add_response("get_object", {"Body": body, "VersionId": "v1"}, params)
    stored = await storage.read("k", "v1", 1000)
    assert stored.data == b"hello" and stored.version == "v1"


@pytest.mark.anyio
async def test_missing_or_changed_object(s3):
    storage, stub = s3
    stub.add_client_error("head_object", "404", http_status_code=404)
    with pytest.raises(ObjectMissingError):
        await storage.read("k", None, 1000)
    stub.add_client_error("head_object", "412", http_status_code=412)
    with pytest.raises(ObjectMissingError):
        await storage.read("k", "etag:old", 1000)


# --- upload API


def stored(id=1, namespace="discord:1", status=DocumentStatus.PENDING_UPLOAD):
    now = datetime.now(UTC)
    return StoredDocument(
        id,
        namespace,
        "a.md",
        None,
        status,
        "s3",
        "bucket",
        "rag/x.md",
        None,
        None,
        None,
        None,
        None,
        0,
        now,
        now,
        None,
    )


class FakeUploads(Uploads):
    def __init__(self):
        super().__init__(None, None, None)
        self.calls = []

    async def create(self, namespace, filename, size_bytes, source=None):
        self.calls.append(("create", namespace, filename, size_bytes, source))
        upload = PresignedUpload("https://s3.invalid/b", {"key": "rag/x.md"}, datetime.now(UTC))
        return stored(namespace=namespace), upload

    async def get(self, namespace, document_id):
        return stored(document_id, namespace) if namespace == "discord:1" else None

    async def list_documents(self, namespace, limit=100):
        return [stored(namespace=namespace)]

    async def delete(self, namespace, document_id):
        self.calls.append(("delete", namespace, document_id))
        return namespace == "discord:1"

    async def reprocess(self, namespace, document_id, force=False):
        self.calls.append(("reprocess", namespace, document_id, force))
        return namespace == "discord:1"


@pytest.fixture
def api(settings):
    from fastapi import FastAPI

    from agent_runtime.adapters.inbound.http_api import build_router, install_error_handlers

    uploads = FakeUploads()
    app = FastAPI()
    install_error_handlers(app)
    app.include_router(build_router(None, FakeModel(), API_KEY, None, uploads))
    client = TestClient(app, headers={"Authorization": f"Bearer {API_KEY}"})
    return client, uploads


def test_upload_endpoint(api):
    client, uploads = api
    body = {"namespace": "discord:1", "filename": "a.md", "size_bytes": 10}
    response = client.post("/v1/knowledge/uploads", json=body)
    assert response.status_code == 201, response.text
    data = response.json()
    assert data["upload"] == {
        "method": "POST",
        "url": "https://s3.invalid/b",
        "fields": {"key": "rag/x.md"},
    }
    assert data["document"]["status"] == "pending_upload"
    assert "object_key" not in data["document"]
    assert uploads.calls == [("create", "discord:1", "a.md", 10, None)]


def test_upload_endpoint_requires_the_key(api):
    client, _ = api
    body = {"namespace": "discord:1", "filename": "a.md", "size_bytes": 10}
    response = client.post("/v1/knowledge/uploads", json=body, headers={"Authorization": "x"})
    assert response.status_code == 401


@pytest.mark.parametrize(
    "body",
    [
        {"namespace": "Discord 1", "filename": "a.md", "size_bytes": 10},
        {"namespace": "discord:1", "filename": "a.md", "size_bytes": 0},
        {"namespace": "discord:1", "filename": "", "size_bytes": 10},
    ],
)
def test_upload_endpoint_validates(api, body):
    client, _ = api
    assert client.post("/v1/knowledge/uploads", json=body).status_code == 422


def test_document_endpoints_are_namespace_scoped(api):
    client, uploads = api
    assert client.get("/v1/knowledge/documents/1?namespace=discord:1").status_code == 200
    assert client.get("/v1/knowledge/documents/1?namespace=discord:2").status_code == 404
    assert client.get("/v1/knowledge/documents/1").status_code == 422
    assert client.get("/v1/knowledge/documents?namespace=discord:1").json()["documents"]
    assert client.delete("/v1/knowledge/documents/1?namespace=discord:2").status_code == 404
    assert client.delete("/v1/knowledge/documents/1?namespace=discord:1").status_code == 204
    response = client.post("/v1/knowledge/documents/1/reprocess?namespace=discord:1&force=true")
    assert response.status_code == 202
    assert ("reprocess", "discord:1", 1, True) in uploads.calls


def test_uploads_disabled_by_default(settings):
    client = TestClient(
        create_app(settings, model=FakeModel()),
        headers={"Authorization": f"Bearer {API_KEY}"},
    )
    body = {"namespace": "discord:1", "filename": "a.md", "size_bytes": 10}
    response = client.post("/v1/knowledge/uploads", json=body)
    assert response.status_code == 404
    assert response.json() == {"error": "uploads disabled"}


# --- configuration


def base(**overrides):
    values = dict(agent_api_key="k", model_base_url="http://m.invalid/v1", model_name="m")
    values.update(overrides)
    return Settings(**values)


def test_postgres_store_needs_a_database_url():
    with pytest.raises(ConfigError, match="DATABASE_URL"):
        base(rag_store="postgres", embedding_provider="hash", embedding_dimensions=1024)


@pytest.mark.parametrize(
    "overrides",
    [
        {"embedding_provider": "hash"},  # 512 by default
        {"embedding_provider": "openai", "embedding_model": "m", "embedding_dimensions": 768},
    ],
)
def test_postgres_store_refuses_other_dimensions(overrides):
    with pytest.raises(ConfigError, match="1024-dimensional"):
        base(rag_store="postgres", database_url="postgres://x", **overrides)


def test_postgres_store_accepts_schema_dimensions():
    s = base(rag_store="postgres", database_url="postgres://x", embedding_model="bge-m3")
    assert s.knowledge_enabled and not s.uploads_enabled


def test_uploads_need_postgres_and_a_queue():
    with pytest.raises(ConfigError, match="RAG_STORE=postgres"):
        base(rag_s3_bucket="b")
    with pytest.raises(ConfigError, match="RAG_SQS_QUEUE_URL"):
        base(
            rag_store="postgres",
            database_url="postgres://x",
            embedding_model="bge-m3",
            rag_s3_bucket="b",
        )


def test_settings_from_env(monkeypatch):
    for key, value in {
        "AGENT_API_KEY": "k",
        "MODEL_BASE_URL": "http://m.invalid/v1",
        "MODEL_NAME": "m",
        "RAG_STORE": "postgres",
        "DATABASE_URL": "postgres://x",
        "EMBEDDING_MODEL": "bge-m3",
        "RAG_S3_BUCKET": "b",
        "RAG_SQS_QUEUE_URL": "https://sqs.invalid/q",
        "RAG_UPLOAD_MAX_BYTES": "2000",
    }.items():
        monkeypatch.setenv(key, value)
    s = Settings.from_env()
    assert s.uploads_enabled and s.rag_upload_max_bytes == 2000 and s.rag_s3_prefix == "rag/"
    assert "postgres://x" not in repr(s)


@pytest.mark.parametrize(
    ("raw", "expected"),
    [(None, False), ("", None), ("omit", None), ("NONE", None), ("true", True), ("false", False)],
)
def test_enable_thinking_from_env(monkeypatch, raw, expected):
    """Unset hides Qwen3 thinking; set but empty omits the field (OpenAI rejects it)."""
    for key, value in {
        "AGENT_API_KEY": "k",
        "MODEL_BASE_URL": "http://m/v1",
        "MODEL_NAME": "m",
    }.items():
        monkeypatch.setenv(key, value)
    if raw is None:
        monkeypatch.delenv("MODEL_ENABLE_THINKING", raising=False)
    else:
        monkeypatch.setenv("MODEL_ENABLE_THINKING", raw)
    assert Settings.from_env().model_enable_thinking is expected


def _model_env(monkeypatch, **extra):
    base = {"AGENT_API_KEY": "k", "MODEL_BASE_URL": "http://m/v1", "MODEL_NAME": "m"}
    for key, value in (base | extra).items():
        monkeypatch.setenv(key, value)


def test_agent_settings_from_env(monkeypatch):
    _model_env(
        monkeypatch,
        AGENT_MAX_MODEL_CALLS="8",
        AGENT_TIME_ZONE="Asia/Tokyo",
        MODEL_PRICE_INPUT_PER_MTOK="0.4",
        MODEL_PRICE_OUTPUT_PER_MTOK="1.6",
        SEARCH_MAX_SEARCHES="2",
    )
    s = Settings.from_env()
    assert (s.agent_max_model_calls, s.agent_time_zone, s.search_max_searches) == (
        8,
        "Asia/Tokyo",
        2,
    )
    assert (s.model_price_input_per_mtok, s.model_price_output_per_mtok) == (0.4, 1.6)


def test_agent_settings_defaults(monkeypatch):
    for key in ("AGENT_MAX_MODEL_CALLS", "AGENT_TIME_ZONE", "MODEL_PRICE_INPUT_PER_MTOK"):
        monkeypatch.delenv(key, raising=False)
    _model_env(monkeypatch)
    s = Settings.from_env()
    assert (s.agent_max_model_calls, s.agent_time_zone) == (10, "Asia/Bangkok")
    assert s.model_price_input_per_mtok is None


@pytest.mark.parametrize(
    ("key", "value", "error"),
    [
        ("AGENT_MAX_MODEL_CALLS", "2", "AGENT_MAX_MODEL_CALLS must be at least 3"),
        ("AGENT_TIME_ZONE", "Mars/Olympus", "unknown AGENT_TIME_ZONE"),
        ("SEARCH_MAX_SEARCHES", "11", "SEARCH_MAX_SEARCHES must be between 0 and 10"),
        ("MODEL_TIMEOUT_SECONDS", "200", "must not exceed TURN_TIMEOUT_SECONDS"),
        ("MODEL_PRICE_OUTPUT_PER_MTOK", "-1", "must not be negative"),
    ],
)
def test_invalid_agent_settings(monkeypatch, key, value, error):
    _model_env(monkeypatch, **{key: value})
    with pytest.raises(ConfigError, match=error):
        Settings.from_env()


# --- hybrid hits in the pipeline


class ScriptedStore(VectorStore):
    shared = True

    def __init__(self, hits):
        self.hits = hits
        self.calls = []

    async def replace(self, chunks, vectors):
        raise AssertionError("no directory configured")

    async def search(self, vector, top_k, *, text="", namespaces=()):
        self.calls.append((text, tuple(namespaces)))
        return self.hits

    async def count(self):
        return len(self.hits)


@pytest.mark.anyio
async def test_keyword_hits_bypass_the_cosine_threshold():
    from agent_runtime.adapters.outbound.hash_embeddings import HashEmbeddings
    from agent_runtime.application import KnowledgeSettings

    def hit(text, score, keyword=None):
        return RetrievedChunk(Chunk("d", "T", "", text, 0), score, keyword)

    store = ScriptedStore([hit("semantic", 0.8), hit("keyword", 0.05, 0.6), hit("noise", 0.05)])
    kb = KnowledgeBase(None, HashEmbeddings(64), store, KnowledgeSettings(min_score=0.3))
    # A shared store is searched although this process indexed nothing.
    kept = await kb.retrieve("/upload limit", ("global", "discord:1"))
    assert [h.chunk.text for h in kept] == ["semantic", "keyword"]
    assert store.calls == [("/upload limit", ("global", "discord:1"))]


@pytest.mark.anyio
async def test_chat_turn_reads_only_its_servers_namespace():
    from agent_runtime.adapters.outbound.hash_embeddings import HashEmbeddings
    from agent_runtime.adapters.outbound.memory_store import InMemoryConversationStore
    from agent_runtime.application import ChatService, ChatSettings

    store = ScriptedStore([])
    kb = KnowledgeBase(None, HashEmbeddings(64), store)
    service = ChatService(
        FakeModel(), InMemoryConversationStore(), ChatSettings(system_prompt="s"), knowledge=kb
    )
    await service.reply("discord:5:10:20", "hello there")
    await service.reply("discord:dm:10:20", "hello there")
    assert [ns for _, ns in store.calls] == [("global", "discord:5"), ("global",)]
