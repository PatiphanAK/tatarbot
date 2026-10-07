import pytest
from conftest import API_KEY, SYSTEM_PROMPT, FakeModel
from fastapi.testclient import TestClient

from agent_runtime.domain import (
    ModelRequestRejectedError,
    ModelResponseError,
    ModelTimeoutError,
    ModelUnavailableError,
    Role,
)
from agent_runtime.main import create_app

AUTH = {"Authorization": f"Bearer {API_KEY}"}
BODY = {"message": "Hello", "conversation_id": "discord:1:2:3"}


@pytest.fixture
def model():
    return FakeModel()


@pytest.fixture
def client(settings, model):
    with TestClient(create_app(settings, model=model)) as c:
        yield c


@pytest.mark.parametrize(
    "headers",
    [
        {},
        {"Authorization": "Bearer wrong"},
        {"Authorization": API_KEY},
        {"Authorization": "Basic x"},
    ],
)
def test_chat_rejects_missing_or_invalid_key(client, model, headers):
    response = client.post("/v1/chat", json=BODY, headers=headers)

    assert response.status_code == 401
    assert response.json() == {"error": "invalid or missing API key"}
    assert response.headers["www-authenticate"] == "Bearer"
    assert model.requests == []


def test_chat_returns_reply(client, model):
    response = client.post("/v1/chat", json=BODY, headers=AUTH)

    assert response.status_code == 200
    assert response.json() == {"message": "reply to: Hello", "conversation_id": "discord:1:2:3"}
    [request] = model.requests
    assert [(m.role, m.content) for m in request.messages] == [
        (Role.SYSTEM, SYSTEM_PROMPT),
        (Role.USER, "Hello"),
    ]


def test_chat_preserves_history_per_conversation(client, model):
    client.post("/v1/chat", json={"message": "first", "conversation_id": "a"}, headers=AUTH)
    client.post("/v1/chat", json={"message": "other", "conversation_id": "b"}, headers=AUTH)
    client.post("/v1/chat", json={"message": "second", "conversation_id": "a"}, headers=AUTH)

    third = model.requests[2]
    assert [(m.role, m.content) for m in third.messages] == [
        (Role.SYSTEM, SYSTEM_PROMPT),
        (Role.USER, "first"),
        (Role.ASSISTANT, "reply to: first"),
        (Role.USER, "second"),
    ]


@pytest.mark.parametrize(
    "body",
    [
        {"conversation_id": "c"},
        {"message": "hi"},
        {"message": "", "conversation_id": "c"},
        {"message": "hi", "conversation_id": "c" * 257},
        {"message": 5, "conversation_id": "c"},
    ],
)
def test_chat_rejects_invalid_body(client, body):
    response = client.post("/v1/chat", json=body, headers=AUTH)

    assert response.status_code == 422
    assert response.json()["error"].startswith("invalid request:")


def test_chat_rejects_blank_message(client, model):
    response = client.post(
        "/v1/chat", json={"message": "   ", "conversation_id": "c"}, headers=AUTH
    )

    assert response.status_code == 422
    assert model.requests == []


def test_chat_rejects_non_json(client):
    response = client.post("/v1/chat", content="not json", headers=AUTH)
    assert response.status_code == 422


@pytest.mark.parametrize(
    ("error", "status", "message"),
    [
        (ModelTimeoutError("t"), 504, "model timed out"),
        (ModelUnavailableError("u"), 503, "model unavailable"),
        (ModelRequestRejectedError("r"), 502, "model rejected the request"),
        (ModelResponseError("m"), 502, "invalid model response"),
    ],
)
def test_model_errors_map_to_5xx_without_leaking(settings, error, status, message):
    model = FakeModel(replies=[error])
    with TestClient(create_app(settings, model=model)) as client:
        response = client.post("/v1/chat", json=BODY, headers=AUTH)

    assert response.status_code == status
    assert response.json() == {"error": message}
    for secret in (API_KEY, settings.model_api_key, settings.model_base_url):
        assert secret not in response.text


def test_unexpected_error_is_500_without_traceback(settings):
    model = FakeModel(replies=[RuntimeError("secret internals")])
    with TestClient(create_app(settings, model=model), raise_server_exceptions=False) as client:
        response = client.post("/v1/chat", json=BODY, headers=AUTH)

    assert response.status_code == 500
    assert response.json() == {"error": "internal error"}
    assert "secret internals" not in response.text


def test_failed_turn_is_not_stored(settings):
    model = FakeModel(replies=[ModelUnavailableError("down"), "ok"])
    with TestClient(create_app(settings, model=model)) as client:
        client.post("/v1/chat", json=BODY, headers=AUTH)
        client.post("/v1/chat", json={**BODY, "message": "retry"}, headers=AUTH)

    assert [m.content for m in model.requests[1].messages] == [SYSTEM_PROMPT, "retry"]


def test_health_needs_no_auth(client):
    response = client.get("/health")
    assert response.status_code == 200
    assert response.json() == {"status": "ok"}


@pytest.mark.parametrize(("ready", "status"), [(True, 200), (False, 503)])
def test_ready_reflects_model(settings, ready, status):
    with TestClient(create_app(settings, model=FakeModel(ready=ready))) as client:
        response = client.get("/ready")

    assert response.status_code == status
    assert response.json()["model"] is ready
