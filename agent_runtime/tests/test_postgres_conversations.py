"""PostgresConversationStore against a real Postgres (`make test-db`)."""

import psycopg
import pytest
from conftest import db

from agent_runtime.adapters.outbound.postgres_conversations import (
    PostgresConversationStore,
    from_json,
    to_json,
)
from agent_runtime.adapters.outbound.postgres_rag import create_pool
from agent_runtime.db.migrate import migrate
from agent_runtime.domain import ChatMessage, Role, ToolCall

pytestmark = pytest.mark.anyio


def msg(text, role=Role.USER):
    return ChatMessage(role, text)


def test_json_round_trip_keeps_tool_calls():
    call = ChatMessage(Role.ASSISTANT, "", (ToolCall("c1", "play_music", '{"q":"chill"}'),))
    result = ChatMessage(Role.TOOL, "pending", tool_call_id="c1")
    for m in (msg("สวัสดี"), call, result):
        assert from_json(to_json(m)) == m


@pytest.fixture
async def store(database):
    migrate(database)
    pool = create_pool(database, 2)
    await pool.open()
    yield PostgresConversationStore(pool, max_messages_per_conversation=3)
    await pool.close()


@db
async def test_unknown_conversation_is_empty(store):
    assert await store.load("nope") == []


@db
async def test_history_survives_a_new_store(store, database):
    await store.append("c", [msg("1"), msg("2", Role.ASSISTANT)])
    pool = create_pool(database, 1)
    await pool.open()
    try:
        fresh = PostgresConversationStore(pool)
        assert [m.content for m in await fresh.load("c")] == ["1", "2"]
    finally:
        await pool.close()


@db
async def test_trims_oldest_messages(store):
    await store.append("c", [msg("1"), msg("2")])
    await store.append("c", [msg("3"), msg("4")])
    assert [m.content for m in await store.load("c")] == ["2", "3", "4"]


@db
async def test_replace_last_matches_roles(store):
    call = ChatMessage(Role.ASSISTANT, "", (ToolCall("c1", "play_music", "{}"),))
    pending = ChatMessage(Role.TOOL, "pending", tool_call_id="c1")
    await store.append("c", [msg("play"), call, pending])
    done = ChatMessage(Role.TOOL, "playing", tool_call_id="c1")
    assert await store.replace_last("c", [done])
    assert (await store.load("c"))[-1] == done
    assert not await store.replace_last("c", [msg("wrong role")])
    assert not await store.replace_last("unknown", [done])


@db
async def test_expired_conversations_are_swept(store, database):
    await store.append("old", [msg("x")])
    with psycopg.connect(database, autocommit=True) as conn:
        conn.execute(
            "UPDATE chat.conversations SET updated_at = now() - interval '31 days'"
            " WHERE conversation_id = 'old'"
        )
    store._last_sweep = 0.0
    await store.append("new", [msg("y")])
    assert await store.load("old") == []
    assert await store.load("new") != []


@db
async def test_database_outage_degrades_to_no_history(database):
    migrate(database)
    pool = create_pool(database, 1)
    await pool.open()
    store = PostgresConversationStore(pool)
    await store.append("c", [msg("1")])
    with psycopg.connect(database, autocommit=True) as conn:
        conn.execute("ALTER TABLE chat.conversations RENAME TO conversations_gone")
    try:
        assert await store.load("c") == []
        await store.append("c", [msg("2")])  # dropped with a warning, no exception
        assert not await store.replace_last("c", [msg("3")])
    finally:
        await pool.close()


# --- configuration and wiring (no database needed)


def settings_with(**overrides):
    from agent_runtime.config import Settings

    values = dict(agent_api_key="k", model_base_url="http://m.invalid/v1", model_name="m")
    values.update(overrides)
    return Settings(**values)


def test_postgres_history_needs_a_database_url():
    from agent_runtime.config import ConfigError

    with pytest.raises(ConfigError, match="CONVERSATION_STORE=postgres"):
        settings_with(conversation_store="postgres")
    with pytest.raises(ConfigError, match="CONVERSATION_STORE must be"):
        settings_with(conversation_store="redis")


def test_postgres_history_does_not_enable_knowledge():
    s = settings_with(conversation_store="postgres", database_url="postgres://x")
    assert s.database_needed and not s.knowledge_enabled


@db
def test_history_survives_a_cold_start(database):
    """Two app instances in a row, like Cloud Run scaling to zero between turns."""
    from conftest import FakeModel
    from fastapi.testclient import TestClient

    from agent_runtime.main import create_app

    migrate(database)
    s = settings_with(conversation_store="postgres", database_url=database)
    auth = {"Authorization": "Bearer k"}

    def turn(client, text):
        body = {"message": text, "conversation_id": "g:1"}
        return client.post("/v1/chat", json=body, headers=auth)

    with TestClient(create_app(s, model=FakeModel())) as first:
        assert turn(first, "first").status_code == 200
    model = FakeModel()
    with TestClient(create_app(s, model=model)) as second:
        assert turn(second, "second").status_code == 200
    seen = [m.content for m in model.requests[0].messages[1:]]
    assert seen == ["first", "reply to: first", "second"]
