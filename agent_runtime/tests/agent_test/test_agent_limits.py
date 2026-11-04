"""Model-call budget, token metering, the local date and what the agent logs."""

import logging
from datetime import UTC, datetime

import pytest
from conftest import SYSTEM_PROMPT
from test_agent import ANSWER_THOUGHT, SEARCH_THOUGHT, FakeSearch, ReActModel, service

from agent_runtime.application.agent import AgentSettings, SearchAgent
from agent_runtime.domain import (
    ChatMessage,
    InferenceResult,
    ModelCallLimitError,
    ModelUnavailableError,
    Role,
    SearchResult,
    TokenUsage,
)

pytestmark = pytest.mark.anyio

USER = (ChatMessage(Role.USER, "weather?"),)


def billed(content, prompt=100, completion=10):
    return InferenceResult(content, usage=TokenUsage(prompt, completion))


# ---- model-call budget


async def test_searches_stop_when_the_call_budget_would_run_out():
    # 4 calls: thought 1 (1), planned search, thought 2 (2) wants another search but only
    # 2 calls are left (< thought + answer + nudge), so it must answer (3).
    model = ReActModel(thoughts=[SEARCH_THOUGHT, "Thought: more.\nNext: SEARCH x"], acts=["ok"])
    search = FakeSearch()

    await service(model, search, max_model_calls=4).chat("c", "hi")

    assert search.queries == ["bangkok weather"]
    assert model.kinds == ["reason", "reason", "act"]
    assert model.acts[0].tools == ()


async def test_default_budget_allows_all_three_searches():
    thoughts = [
        "Thought: a.\nNext: SEARCH q1",
        "Thought: b.\nNext: SEARCH q2",
        "Thought: c.\nNext: SEARCH q3",
        ANSWER_THOUGHT,
    ]
    model = ReActModel(thoughts=thoughts, acts=["answer"])
    search = FakeSearch()

    await service(model, search).chat("c", "hi")

    assert search.queries == ["q1", "q2", "q3"]
    assert len(model.requests) == 5  # was 8 with a model call per search


async def test_call_beyond_the_budget_raises_and_is_logged(caplog):
    # A budget below the agent's minimum, to reach the backstop: thought (1), answer (2).
    model = ReActModel(thoughts=[ANSWER_THOUGHT], acts=["never sent"])
    agent = SearchAgent(model, FakeSearch(), AgentSettings(max_model_calls=1))

    with caplog.at_level(logging.WARNING), pytest.raises(ModelCallLimitError):
        await agent.run(USER, "rid")

    assert len(model.requests) == 1
    assert "error=ModelCallLimitError model_calls=1 failed_calls=0" in caplog.text


def test_call_limit_maps_to_502(settings):
    from conftest import API_KEY, FakeModel
    from fastapi.testclient import TestClient

    from agent_runtime.main import create_app

    model = FakeModel(replies=[ModelCallLimitError("turn reached 10 model calls")])
    with TestClient(create_app(settings, model=model)) as client:
        response = client.post(
            "/v1/chat",
            json={"message": "hi", "conversation_id": "c"},
            headers={"Authorization": f"Bearer {API_KEY}"},
        )

    assert response.status_code == 502
    assert response.json() == {"error": "model call limit reached"}


# ---- tokens


async def test_tokens_are_summed_over_the_turn():
    model = ReActModel(
        thoughts=[billed(SEARCH_THOUGHT, 100, 20), billed(ANSWER_THOUGHT, 300, 15)],
        acts=[billed("32°C", 400, 30)],
    )
    agent = SearchAgent(model, FakeSearch(), AgentSettings())

    result = await agent.run(USER, "rid")

    assert (result.model_calls, result.input_tokens, result.output_tokens) == (3, 800, 65)


async def test_tokens_are_unknown_if_a_call_reports_no_usage():
    model = ReActModel(thoughts=[billed(ANSWER_THOUGHT)], acts=["no usage"])
    agent = SearchAgent(model, FakeSearch(), AgentSettings())

    result = await agent.run(USER, "rid")

    assert (result.input_tokens, result.output_tokens) == (None, None)


async def test_failed_turn_logs_the_calls_already_made(caplog):
    model = ReActModel(thoughts=[billed(ANSWER_THOUGHT, 120, 8)], acts=[ModelUnavailableError()])
    agent = SearchAgent(model, FakeSearch(), AgentSettings())

    with caplog.at_level(logging.WARNING), pytest.raises(ModelUnavailableError):
        await agent.run(USER, "rid")

    # Both calls were sent; tokens are those of the call that completed.
    assert (
        "error=ModelUnavailableError model_calls=2 failed_calls=1 input_tokens=120 output_tokens=8"
    ) in caplog.text


async def test_concurrent_turns_have_separate_meters():
    import asyncio

    def model():
        return ReActModel(thoughts=[billed(ANSWER_THOUGHT, 10, 1)], acts=[billed("hi", 10, 1)])

    agents = [SearchAgent(model(), FakeSearch(), AgentSettings()) for _ in range(5)]
    results = await asyncio.gather(*(a.run(USER, f"r{i}") for i, a in enumerate(agents)))

    assert {(r.input_tokens, r.output_tokens) for r in results} == {(20, 2)}


# ---- per-turn log: counts, tokens, cost; never content


async def test_turn_log_has_tokens_and_cost(caplog):
    model = ReActModel(
        thoughts=[billed(SEARCH_THOUGHT, 1000, 50), billed(ANSWER_THOUGHT, 2000, 50)],
        acts=[billed("ok", 3000, 100)],
    )
    chat = service(model, FakeSearch(), price_input_per_mtok=0.4, price_output_per_mtok=1.6)

    with caplog.at_level(logging.INFO):
        await chat.chat("c", "my secret question about bangkok weather")

    line = next(r.getMessage() for r in caplog.records if "chat turn completed" in r.message)
    # 6000 in * 0.4 + 200 out * 1.6, per 1M tokens.
    assert "searches=1 actions=0 model_calls=3 input_tokens=6000 output_tokens=200" in line
    assert "cost_usd=0.002720" in line


async def test_no_cost_without_prices(caplog):
    model = ReActModel(thoughts=[billed(ANSWER_THOUGHT)], acts=[billed("ok")])

    with caplog.at_level(logging.INFO):
        await service(model, FakeSearch()).chat("c", "hi")

    assert "input_tokens=200 output_tokens=20 cost_usd=- " in caplog.text


async def test_info_logs_carry_no_conversation_content(caplog):
    model = ReActModel(
        thoughts=["Thought: PRIVATE-THOUGHT.\nNext: SEARCH private-query", ANSWER_THOUGHT],
        acts=["PRIVATE-REPLY"],
    )

    with caplog.at_level(logging.INFO):
        await service(model, FakeSearch()).chat("c", "PRIVATE-MESSAGE")

    for secret in ("PRIVATE-THOUGHT", "private-query", "PRIVATE-REPLY", "PRIVATE-MESSAGE"):
        assert secret not in caplog.text
    assert "decision=search query_chars=13" in caplog.text

    caplog.clear()
    model = ReActModel(thoughts=["Thought: t.\nNext: SEARCH debug-query"], acts=["x"])
    with caplog.at_level(logging.DEBUG, logger="agent_runtime.application.agent"):
        await service(model, FakeSearch()).chat("c2", "hi")
    assert "debug-query" in caplog.text  # available when debugging


# ---- local date


def frozen(iso):
    return lambda: datetime.fromisoformat(iso).astimezone(UTC)


def test_instructions_use_the_bangkok_date():
    # 18:48 UTC on 4 Nov is already 01:48 on 5 Nov in Bangkok.
    agent = SearchAgent(
        ReActModel(), FakeSearch(), AgentSettings(now=frozen("2026-11-04T18:48:00+00:00"))
    )

    text = agent.instructions()

    assert "It is now Thursday 2026-11-05 01:48 in Asia/Bangkok (UTC+07:00)." in text
    assert "2026-11-04" not in text


def test_time_zone_is_configurable():
    agent = SearchAgent(
        ReActModel(),
        FakeSearch(),
        AgentSettings(time_zone="UTC", now=frozen("2026-11-04T18:48:00+00:00")),
    )

    assert "Wednesday 2026-11-04 18:48 in UTC (UTC+00:00)" in agent.instructions()


async def test_system_message_carries_the_local_date():
    model = ReActModel(thoughts=[ANSWER_THOUGHT], acts=["ok"])
    chat = service(model, FakeSearch())
    chat._agent._settings = AgentSettings(now=frozen("2026-12-31T17:30:00+00:00"))

    await chat.chat("c", "what's new today?")

    system = model.requests[0].messages[0]
    assert system.role is Role.SYSTEM and system.content.startswith(SYSTEM_PROMPT)
    assert "Friday 2027-01-01 00:30 in Asia/Bangkok" in system.content


# ---- stale results: today's date next to each result's own date


def test_instructions_say_how_to_treat_older_results():
    text = SearchAgent(ReActModel(), FakeSearch(), AgentSettings()).instructions()

    assert "Results can be older than today" in text
    assert "never call it today's" in text
    assert "today's figure was not found" in text


async def test_observation_shows_today_and_each_published_date():
    results = [
        SearchResult("Gold 1 Nov", "https://a.example", "62,250 baht", published="2026-11-01"),
        SearchResult("Gold", "https://b.example", "price page"),
    ]
    model = ReActModel(thoughts=[SEARCH_THOUGHT], acts=["ok"])
    chat = service(model, FakeSearch(results=results))
    chat._agent._settings = AgentSettings(now=frozen("2026-11-04T18:48:00+00:00"))

    await chat.chat("c", "ราคาทองคำวันนี้เท่าไร")

    observation = model.reasons[1].messages[-2].content
    assert "Today is Thursday 2026-11-05." in observation
    assert "[1] Gold 1 Nov\nURL: https://a.example\nPublished: 2026-11-01\n62,250 baht" in (
        observation
    )
    assert "Published: unknown" in observation


async def test_no_results_still_states_today():
    model = ReActModel(thoughts=[SEARCH_THOUGHT], acts=["nothing found"])
    chat = service(model, FakeSearch(results=[]))
    chat._agent._settings = AgentSettings(now=frozen("2026-11-04T18:48:00+00:00"))

    await chat.chat("c", "news?")

    assert "No results for 'bangkok weather' (today is Thursday 2026-11-05)." in (
        model.reasons[1].messages[-2].content
    )
