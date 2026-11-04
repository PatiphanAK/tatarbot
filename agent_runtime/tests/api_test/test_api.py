import pytest
from conftest import API_KEY, SYSTEM_EN, FakeModel
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
    assert response.json() == {
        "message": "reply to: Hello",
        "conversation_id": "discord:1:2:3",
        "actions": [],
    }
    [request] = model.requests
    assert [(m.role, m.content) for m in request.messages] == [
        (Role.SYSTEM, SYSTEM_EN),
        (Role.USER, "Hello"),
    ]


def test_chat_preserves_history_per_conversation(client, model):
    client.post("/v1/chat", json={"message": "first", "conversation_id": "a"}, headers=AUTH)
    client.post("/v1/chat", json={"message": "other", "conversation_id": "b"}, headers=AUTH)
    client.post("/v1/chat", json={"message": "second", "conversation_id": "a"}, headers=AUTH)

    third = model.requests[2]
    assert [(m.role, m.content) for m in third.messages] == [
        (Role.SYSTEM, SYSTEM_EN),
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

    assert [m.content for m in model.requests[1].messages] == [SYSTEM_EN, "retry"]


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


# --- OWASP API8: no API description in production


@pytest.mark.parametrize("path", ["/docs", "/redoc", "/openapi.json", "/docs/oauth2-redirect"])
def test_api_docs_are_off_by_default(client, path):
    assert client.get(path).status_code == 404


def test_api_docs_can_be_enabled_for_development(settings, model):
    import dataclasses

    dev = dataclasses.replace(settings, api_docs_enabled=True)
    with TestClient(create_app(dev, model=model)) as c:
        assert c.get("/openapi.json").status_code == 200
        assert c.get("/docs").status_code == 200
        assert c.get("/redoc").status_code == 200


def test_api_docs_setting_defaults_to_off(monkeypatch):
    from agent_runtime.config import Settings

    for key, value in {
        "AGENT_API_KEY": "k",
        "MODEL_BASE_URL": "http://m.invalid/v1",
        "MODEL_NAME": "m",
    }.items():
        monkeypatch.setenv(key, value)
    monkeypatch.delenv("API_DOCS_ENABLED", raising=False)
    assert Settings.from_env().api_docs_enabled is False
    monkeypatch.setenv("API_DOCS_ENABLED", "true")
    assert Settings.from_env().api_docs_enabled is True
