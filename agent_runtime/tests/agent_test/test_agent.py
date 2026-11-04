import asyncio
import json

import pytest
from conftest import SYSTEM_PROMPT, FakeModel

from agent_runtime.adapters.outbound.memory_store import InMemoryConversationStore
from agent_runtime.application import ChatService, ChatSettings, ModelPort, SearchPort
from agent_runtime.application.agent import (
    MAX_QUERY_CHARS,
    REASON_MAX_TOKENS,
    WEB_SEARCH,
    AgentSettings,
    Decision,
    ReActStep,
    SearchAgent,
    _parse_thought,
)
from agent_runtime.domain import (
    ChatMessage,
    InferenceRequest,
    InferenceResult,
    ModelTimeoutError,
    ModelUnavailableError,
    Role,
    SearchError,
    SearchResult,
    ToolCall,
)

pytestmark = pytest.mark.anyio

RESULTS = [
    SearchResult("Weather A", "https://a.example/w", "32°C, sunny"),
    SearchResult("Weather B", "https://b.example/w", "33°C, humid"),
]
REASON_MARK = "[Agent step, not from the user] Think before acting"
ACT_MARK = "[Agent step, not from the user] Your reasoning:"

SEARCH_THOUGHT = (
    "Thought: The user wants current weather; I don't know it.\nNext: SEARCH bangkok weather"
)
# SEARCH without a query: the model is asked to make the web_search call itself.
SEARCH_NO_QUERY = "Thought: I need to look this up.\nNext: SEARCH"
ANSWER_THOUGHT = "Thought: The results say 32°C and sunny.\nNext: ANSWER"


class FakeSearch(SearchPort):
    def __init__(self, results=RESULTS, error=None, delay=0.0):
        self.queries: list[str] = []
        self._results = results
        self._error = error
        self._delay = delay

    async def search(self, query, max_results):
        self.queries.append(query)
        await asyncio.sleep(self._delay)
        if self._error:
            raise self._error
        return list(self._results)[:max_results]


class ReActModel(ModelPort):
    """Scripted separately for reasoning calls and acting calls (str, result or error)."""

    def __init__(self, thoughts=(), acts=()):
        self.requests: list[InferenceRequest] = []
        self._thoughts = list(thoughts)
        self._acts = list(acts)

    @staticmethod
    def is_reason(request: InferenceRequest) -> bool:
        return request.messages[-1].content.startswith(REASON_MARK)

    @property
    def kinds(self) -> list[str]:
        return ["reason" if self.is_reason(r) else "act" for r in self.requests]

    @property
    def acts(self) -> list[InferenceRequest]:
        return [r for r in self.requests if not self.is_reason(r)]

    @property
    def reasons(self) -> list[InferenceRequest]:
        return [r for r in self.requests if self.is_reason(r)]

    async def generate(self, request):
        self.requests.append(request)
        script = self._thoughts if self.is_reason(request) else self._acts
        reply = script.pop(0) if script else ANSWER_THOUGHT if self.is_reason(request) else "ok"
        if isinstance(reply, Exception):
            raise reply
        return reply if isinstance(reply, InferenceResult) else InferenceResult(reply)

    async def is_ready(self):
        return True


def search_call(query="bangkok weather", call_id="call_1"):
    return ToolCall(call_id, "web_search", json.dumps({"query": query}))


def wants(*calls):
    return InferenceResult(content="", finish_reason="tool_calls", tool_calls=calls)


def service(model, search=None, store=None, **settings):
    return ChatService(
        model,
        store or InMemoryConversationStore(),
        ChatSettings(system_prompt=SYSTEM_PROMPT, **settings),
        search=search,
    )


# ---- without search: no ReAct stage


async def test_without_search_port_one_plain_call():
    model = FakeModel()
    await service(model).chat("c", "hello")

    [request] = model.requests
    assert request.tools == ()
    assert "web_search" not in request.messages[0].content
    assert request.messages[-1] == ChatMessage(Role.USER, "hello")


async def test_zero_max_searches_disables_the_react_stage():
    model = FakeModel()
    await service(model, FakeSearch(), max_searches=0).chat("c", "hi")

    [request] = model.requests
    assert request.tools == ()


# ---- Thought -> Action -> Observation


async def test_full_react_loop():
    model = ReActModel(
        thoughts=[SEARCH_THOUGHT, ANSWER_THOUGHT], acts=["It is about 32°C and sunny."]
    )
    search, store = FakeSearch(), InMemoryConversationStore()

    reply = await service(model, search, store).chat("c", "What's the weather in Bangkok?")

    # The thought's query is searched as written: no model call to repeat it.
    assert model.kinds == ["reason", "reason", "act"]
    assert search.queries == ["bangkok weather"]

    # Thought: no tools, short budget, sees the conversation.
    first_reason, second_reason = model.reasons
    assert first_reason.tools == ()
    assert first_reason.params.max_tokens == REASON_MAX_TOKENS
    assert "You can still search 3 more time(s)." in first_reason.messages[-1].content
    assert first_reason.messages[-2] == ChatMessage(Role.USER, "What's the weather in Bangkok?")

    # Action: the planned search appears in the scratchpad as an assistant tool call...
    planned = second_reason.messages[-3]
    assert planned.role is Role.ASSISTANT and planned.content == ""
    [call] = planned.tool_calls
    assert (call.name, json.loads(call.arguments)) == ("web_search", {"query": "bangkok weather"})

    # ...and its observation (answering that call) feeds the next thought, which also sees
    # the earlier one.
    assert second_reason.messages[-2].role is Role.TOOL
    assert second_reason.messages[-2].tool_call_id == call.id
    assert "https://a.example/w" in second_reason.messages[-2].content
    assert "- The user wants current weather; I don't know it." in (
        second_reason.messages[-1].content
    )
    assert "You can still search 2 more time(s)." in second_reason.messages[-1].content

    # The thought chose ANSWER: no tool offered.
    [second_act] = model.acts
    assert second_act.tools == ()
    assert "write the final answer" in second_act.messages[-1].content

    # Thoughts and step prompts never reach the user or history.
    assert reply == (
        "It is about 32°C and sunny.\n\nSources:\n- <https://a.example/w>\n- <https://b.example/w>"
    )
    assert await store.load("c") == [
        ChatMessage(Role.USER, "What's the weather in Bangkok?"),
        ChatMessage(Role.ASSISTANT, "It is about 32°C and sunny."),
    ]
    # Request ids number every model call of the turn.
    assert [r.request_id.rsplit(".", 1)[1] for r in model.requests] == ["1", "2", "3"]


async def test_search_without_a_query_asks_the_model_for_the_call():
    """The previous behaviour, kept when the thought names no query."""
    model = ReActModel(
        thoughts=[SEARCH_NO_QUERY, ANSWER_THOUGHT],
        acts=[wants(search_call("model query")), "It is sunny."],
    )
    search = FakeSearch()

    await service(model, search).chat("c", "weather?")

    assert model.kinds == ["reason", "act", "reason", "act"]
    assert search.queries == ["model query"]
    first_act = model.acts[0]
    assert first_act.tools == (WEB_SEARCH,)
    assert first_act.messages[-1].content.startswith(ACT_MARK)
    assert "your own keywords" in first_act.messages[-1].content


async def test_planned_query_keeps_thai_and_is_bounded():
    long_query = "ราคาทองคำวันนี้ " + "x" * 400
    model = ReActModel(thoughts=[f"Thought: gold.\nNext: SEARCH {long_query}"], acts=["ok"])
    search = FakeSearch()

    await service(model, search).chat("c", "ราคาทองคำวันนี้เท่าไร")

    [query] = search.queries
    assert query == long_query[:MAX_QUERY_CHARS]
    planned = model.reasons[1].messages[-3].tool_calls[0]
    assert "ราคาทองคำ" in planned.arguments  # not \\u-escaped


async def test_each_planned_search_gets_its_own_call_id():
    model = ReActModel(
        thoughts=[SEARCH_THOUGHT, "Thought: more.\nNext: SEARCH rain radar", ANSWER_THOUGHT],
        acts=["done"],
    )

    await service(model, FakeSearch()).chat("c", "weather?")

    final = model.acts[0].messages
    ids = [c.id for m in final if m.role is Role.ASSISTANT for c in m.tool_calls]
    answered = [m.tool_call_id for m in final if m.role is Role.TOOL]
    assert len(set(ids)) == 2 and answered == ids


async def test_agent_returns_the_react_trace():
    model = ReActModel(thoughts=[SEARCH_THOUGHT, ANSWER_THOUGHT], acts=["32°C"])
    agent = SearchAgent(model, FakeSearch(), AgentSettings())

    result = await agent.run((ChatMessage(Role.USER, "weather?"),), "rid")

    assert result.steps == (
        ReActStep(
            "The user wants current weather; I don't know it.",
            Decision.SEARCH,
            "search: bangkok weather",
        ),
        ReActStep("The results say 32°C and sunny.", Decision.ANSWER, "answer"),
    )
    assert (result.searches, result.model_calls) == (1, 3)


async def test_answer_thought_skips_search():
    model = ReActModel(thoughts=["Thought: Just a greeting.\nNext: ANSWER"], acts=["Hi there!"])
    search = FakeSearch()

    assert await service(model, search).chat("c", "hello") == "Hi there!"

    assert model.kinds == ["reason", "act"]
    assert model.acts[0].tools == ()
    assert search.queries == []


async def test_unclear_thought_lets_the_model_choose():
    model = ReActModel(thoughts=["I am not sure what to do."], acts=["answer"])
    await service(model, FakeSearch()).chat("c", "hmm")

    act = model.acts[0]
    assert act.tools == (WEB_SEARCH,)
    assert "call a tool if you need one" in act.messages[-1].content


async def test_budget_spent_forces_an_answer():
    model = ReActModel(
        thoughts=[SEARCH_THOUGHT, "Thought: need more.\nNext: SEARCH more weather"],
        acts=["done"],
    )
    search = FakeSearch()

    reply = await service(model, search, max_searches=1).chat("c", "hi")

    assert search.queries == ["bangkok weather"]
    assert "No searches left: you must answer now." in model.reasons[1].messages[-1].content
    # SEARCH with no budget left becomes ANSWER.
    [act] = model.acts
    assert act.tools == ()
    assert "write the final answer" in act.messages[-1].content
    assert reply.startswith("done")


@pytest.mark.parametrize(
    "stubborn",
    [wants(search_call("q2")), InferenceResult('<tool_call>{"name": "web_search"}</tool_call>')],
)
async def test_tool_call_without_offer_is_retried_once_with_a_nudge(stubborn):
    model = ReActModel(
        thoughts=["Thought: I know this.\nNext: ANSWER"], acts=[stubborn, "final answer"]
    )

    reply = await service(model, FakeSearch()).chat("c", "hi")

    assert model.kinds == ["reason", "act", "act"]
    nudge = model.requests[2]
    assert nudge.tools == () and "Do not call any tools" in nudge.messages[-1].content
    assert reply == "final answer"


async def test_leaked_scaffold_is_stripped_from_the_answer():
    model = ReActModel(acts=["Thought: say hi\nNext: ANSWER\nHello! 👋"])

    assert await service(model, FakeSearch()).chat("c", "hi") == "Hello! 👋"


async def test_parallel_calls_beyond_budget_are_refused():
    calls = (search_call("a", "c1"), search_call("b", "c2"), search_call("c", "c3"))
    model = ReActModel(thoughts=[SEARCH_NO_QUERY], acts=[wants(*calls), "answer"])
    search = FakeSearch()

    await service(model, search, max_searches=2).chat("c", "hi")

    assert search.queries == ["a", "b"]
    tool_messages = [m for m in model.reasons[1].messages if m.role is Role.TOOL]
    assert [m.tool_call_id for m in tool_messages] == ["c1", "c2", "c3"]
    assert "search limit reached" in tool_messages[2].content
    assert model.acts[1].tools == ()


async def test_search_failure_is_observed_not_raised():
    model = ReActModel(thoughts=[SEARCH_THOUGHT], acts=["Could not check."])
    search = FakeSearch(error=SearchError("search failed (HTTP 500)"))

    reply = await service(model, search).chat("c", "news?")

    observation = model.reasons[1].messages[-2]
    assert "unavailable" in observation.content and "HTTP 500" not in observation.content
    assert reply == "Could not check."


@pytest.mark.parametrize(
    ("call", "error"),
    [
        (ToolCall("c1", "web_search", "not json"), 'call web_search with {"query"'),
        (ToolCall("c1", "web_search", '{"query": "  "}'), 'call web_search with {"query"'),
    ],
)
async def test_bad_tool_calls_get_an_error_observation(call, error):
    model = ReActModel(thoughts=[SEARCH_NO_QUERY], acts=[wants(call), "ok"])
    search = FakeSearch()

    await service(model, search).chat("c", "hi")

    assert search.queries == []
    assert error in model.reasons[1].messages[-2].content


@pytest.mark.parametrize("stage", ["reason", "act"])
async def test_model_errors_propagate_from_any_stage(stage):
    error = ModelUnavailableError("down")
    model = ReActModel(thoughts=[error] if stage == "reason" else [], acts=[error])
    store = InMemoryConversationStore()

    with pytest.raises(ModelUnavailableError):
        await service(model, FakeSearch(), store).chat("c", "hi")
    assert await store.load("c") == []


async def test_turn_timeout_covers_the_whole_loop():
    model = ReActModel(thoughts=[SEARCH_THOUGHT], acts=["late"])

    with pytest.raises(ModelTimeoutError):
        await service(model, FakeSearch(delay=1), turn_timeout_seconds=0.05).chat("c", "hi")


async def test_thai_turn_gets_thai_sources_label_and_skips_cited_urls():
    model = ReActModel(thoughts=[SEARCH_THOUGHT], acts=["ร้อนมาก ดูที่ https://a.example/w"])

    reply = await service(model, FakeSearch()).chat("c", "อากาศกรุงเทพวันนี้เป็นยังไง")

    assert reply.endswith("แหล่งที่มา:\n- <https://b.example/w>")
    assert reply.count("https://a.example/w") == 1


# ---- parsing the thought


@pytest.mark.parametrize(
    ("text", "expected"),
    [
        (
            SEARCH_THOUGHT,
            ("The user wants current weather; I don't know it.", "search", "bangkok weather"),
        ),
        (ANSWER_THOUGHT, ("The results say 32°C and sunny.", "answer", "")),
        (
            '**Thought:** hm\n**Next:** SEARCH: "SET index today"',
            ("hm", "search", "SET index today"),
        ),
        ("thought: a\nnext: answer\nNext: SEARCH x", ("a\nnext: answer", "search", "x")),
        ("no structure at all", ("no structure at all", "unclear", "")),
        # The decision at the end of the thought's last line (seen with Qwen3 in the eval).
        (
            "The user wants a fact I know. No search needed. Next: ANSWER",
            ("The user wants a fact I know. No search needed.", "answer", ""),
        ),
        (
            "Wants gold prices, which change daily. Next: SEARCH gold price Thailand today",
            ("Wants gold prices, which change daily.", "search", "gold price Thailand today"),
        ),
        ("Wants lo-fi for studying. **Next:** MUSIC", ("Wants lo-fi for studying.", "music", "")),
        # A line-start decision wins over an inline one.
        (
            "I said Next: SEARCH x before.\nNext: ANSWER",
            ("I said Next: SEARCH x before.", "answer", ""),
        ),
        # Inside prose without the strict form, it is not a decision.
        (
            "I will answer next: search later maybe",
            ("I will answer next: search later maybe", "unclear", ""),
        ),
        (
            "The next search should cover prices.",
            ("The next search should cover prices.", "unclear", ""),
        ),
    ],
)
def test_parse_thought(text, expected):
    assert _parse_thought(text) == expected


async def test_calls_to_tools_that_were_not_offered_are_ignored():
    model = ReActModel(
        thoughts=[SEARCH_NO_QUERY],
        acts=[wants(ToolCall("c1", "rm_rf", "{}")), "Here is what I know."],
    )
    search = FakeSearch()

    reply = await service(model, search).chat("c", "hi")

    assert reply == "Here is what I know."
    assert search.queries == []
    assert model.kinds == ["reason", "act", "act"]  # the second act is the nudge
