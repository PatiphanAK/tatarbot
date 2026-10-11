import json

import pytest
from conftest import API_KEY, SYSTEM_PROMPT, FakeModel
from fastapi.testclient import TestClient

from agent_runtime.adapters.outbound.memory_store import InMemoryConversationStore
from agent_runtime.application import ChatService, ChatSettings, SearchPort
from agent_runtime.domain import ChatMessage, InferenceResult, Role, SearchResult, ToolCall
from agent_runtime.domain.client_tools import (
    PLAY_MUSIC,
    ClientAction,
    ClientTools,
    MusicAction,
    MusicTool,
    Station,
    parse_play_music,
    play_music_spec,
)
from agent_runtime.main import create_app

MUSIC = MusicTool((Station("lofi", "Lo-fi beats"), Station("jazz", "Modern jazz")))
TOOLS = ClientTools(music=MUSIC)


def music_call(args, call_id="m1"):
    return ToolCall(call_id, PLAY_MUSIC, json.dumps(args))


def wants(*calls, content=""):
    return InferenceResult(content=content, finish_reason="tool_calls", tool_calls=calls)


# ---- domain


def test_spec_limits_arguments_to_the_callers_stations():
    spec = play_music_spec(MUSIC)
    props = spec.parameters["properties"]
    assert props["station"]["enum"] == ["lofi", "jazz"]
    assert props["action"]["enum"] == ["play", "skip", "stop", "queue"]
    assert "lofi: Lo-fi beats" in spec.description


@pytest.mark.parametrize(
    ("args", "expected"),
    [
        ({"action": "play", "station": "jazz"}, ClientAction(MusicAction.PLAY, "jazz")),
        ({"action": "PLAY", "station": " Lofi "}, ClientAction(MusicAction.PLAY, "lofi")),
        ({"action": "skip"}, ClientAction(MusicAction.SKIP)),
        ({"action": "stop", "station": "ignored"}, ClientAction(MusicAction.STOP)),
        ({"action": "play", "station": "polka"}, None),  # not a listed station
        ({"action": "play"}, None),
        ({"action": "volume_up"}, None),
        ({"station": "lofi"}, None),
    ],
)
def test_arguments_are_validated(args, expected):
    assert parse_play_music(music_call(args), MUSIC) == expected


def test_malformed_arguments_are_rejected():
    assert parse_play_music(ToolCall("m", PLAY_MUSIC, "not json"), MUSIC) is None
    assert parse_play_music(ToolCall("m", "web_search", "{}"), MUSIC) is None


# ---- through ChatService, without web search (one model call)


def service(model, store=None, search=None):
    return ChatService(
        model,
        store or InMemoryConversationStore(),
        ChatSettings(system_prompt=SYSTEM_PROMPT),
        search=search,
    )


@pytest.mark.anyio
async def test_music_request_returns_an_action_without_running_it():
    model = FakeModel(replies=[wants(music_call({"action": "play", "station": "lofi"}))])
    store = InMemoryConversationStore()

    reply = await service(model, store).reply("c", "เปิดเพลงชิลหน่อย", TOOLS)

    assert reply.actions == (ClientAction(MusicAction.PLAY, "lofi"),)
    assert reply.message == ""
    [request] = model.requests  # no second model call: the bot carries it out
    assert [t.name for t in request.tools] == [PLAY_MUSIC]
    assert "play_music" in request.messages[0].content  # music instructions
    # History remembers the request neutrally, not as a success.
    assert (await store.load("c"))[-1] == ChatMessage(Role.ASSISTANT, "[music: play lofi]")


@pytest.mark.anyio
async def test_text_alongside_the_action_is_kept():
    model = FakeModel(replies=[wants(music_call({"action": "skip"}), content="Skipping!")])

    reply = await service(model).reply("c", "skip this", TOOLS)

    assert (reply.message, reply.actions) == ("Skipping!", (ClientAction(MusicAction.SKIP),))


@pytest.mark.anyio
async def test_without_client_tools_nothing_is_offered():
    model = FakeModel()

    reply = await service(model).reply("c", "play lofi")

    assert reply.actions == ()
    assert model.requests[0].tools == ()
    assert "play_music" not in model.requests[0].messages[0].content


@pytest.mark.anyio
async def test_invalid_music_arguments_fall_back_to_an_answer():
    model = FakeModel(
        replies=[
            wants(music_call({"action": "play", "station": "polka"})),
            "I can play lofi or jazz.",
        ]
    )

    reply = await service(model).reply("c", "play polka", TOOLS)

    assert reply.actions == ()
    assert reply.message == "I can play lofi or jazz."
    assert model.requests[1].tools == ()  # the nudge


# ---- with web search: the thought decides


class NoSearch(SearchPort):
    async def search(self, query, max_results):
        raise AssertionError("must not search")


@pytest.mark.anyio
async def test_music_thought_offers_only_play_music():
    model = FakeModel(
        replies=[
            "Thought: The user wants jazz radio.\nNext: MUSIC",
            wants(music_call({"action": "play", "station": "jazz"})),
        ]
    )

    reply = await service(model, search=NoSearch()).reply("c", "put on some jazz", TOOLS)

    reason, act = model.requests
    assert "Next: MUSIC" in reason.messages[-1].content
    assert [t.name for t in act.tools] == [PLAY_MUSIC]
    assert reply.actions == (ClientAction(MusicAction.PLAY, "jazz"),)


@pytest.mark.anyio
async def test_music_is_not_an_option_without_the_tool():
    model = FakeModel(replies=["Thought: hmm.\nNext: MUSIC", "Sorry, I can't play music here."])

    reply = await service(model, search=NoSearch()).reply("c", "play jazz")

    reason, act = model.requests
    assert "Next: MUSIC" not in reason.messages[-1].content
    assert all(t.name != PLAY_MUSIC for t in act.tools)
    assert reply.actions == ()


@pytest.mark.anyio
async def test_search_thought_never_offers_play_music():
    class OneResult(SearchPort):
        async def search(self, query, max_results):
            return [SearchResult("A", "https://a.example", "x")]

    model = FakeModel(
        replies=[
            "Thought: need news.\nNext: SEARCH thai news",
            wants(ToolCall("s1", "web_search", '{"query": "thai news"}')),
            "Thought: done.\nNext: ANSWER",
            "Here's the news.",
        ]
    )

    await service(model, search=OneResult()).reply("c", "news?", TOOLS)

    assert [t.name for t in model.requests[1].tools] == ["web_search"]


# ---- HTTP


@pytest.fixture
def client(settings):
    model = FakeModel(replies=[wants(music_call({"action": "play", "station": "lofi"}))])
    with TestClient(create_app(settings, model=model)) as c:
        c.model = model
        yield c


AUTH = {"Authorization": f"Bearer {API_KEY}"}
STATIONS = [{"name": "lofi", "title": "Lo-fi"}, {"name": "jazz", "title": "Jazz"}]


def test_http_returns_actions(client):
    body = {
        "message": "play lofi",
        "conversation_id": "discord:1:2:3",
        "client_tools": {"music": {"stations": STATIONS}},
    }

    response = client.post("/v1/chat", json=body, headers=AUTH)

    assert response.status_code == 200
    assert response.json() == {
        "message": "",
        "conversation_id": "discord:1:2:3",
        "actions": [{"type": "play_music", "action": "play", "station": "lofi"}],
    }


@pytest.mark.parametrize(
    "stations",
    [
        [],
        [{"name": "Lo Fi", "title": "x"}],
        [{"name": "lofi", "title": ""}],
        [{"name": "lofi", "title": "a"}, {"name": "lofi", "title": "b"}],
        [{"name": f"s{i}", "title": "t"} for i in range(26)],
    ],
)
def test_http_validates_client_tools(client, stations):
    body = {
        "message": "play",
        "conversation_id": "c",
        "client_tools": {"music": {"stations": stations}},
    }
    assert client.post("/v1/chat", json=body, headers=AUTH).status_code == 422
