"""Agent eval: runs every case of `dataset.jsonl` through the real `ChatService` and model,
scores it (`scoring.py`) and logs the run to MLflow. See `evals/README.md`.

    make -C agent_runtime eval                       # all cases, canned search
    uv run --group eval python -m evals.run --category music-play --limit 5

Each case is one turn in a fresh conversation, with the bot's music tool offered (as the
bot does in a server) and web search enabled. Search is canned by default, so a run is
cheap and repeatable; `--live-search` uses Tavily instead. The knowledge base (RAG) and
the input guardrail are not part of this eval.
"""

import argparse
import asyncio
import hashlib
import json
import os
import subprocess
import sys
import time
import tomllib
from collections import defaultdict
from pathlib import Path
from urllib.parse import urlsplit

os.environ.setdefault("MLFLOW_DISABLE_AGENT_HINT", "1")

import mlflow  # noqa: E402
from mlflow.entities import SpanType  # noqa: E402
from mlflow.genai.scorers import scorer  # noqa: E402

from agent_runtime.adapters.outbound.memory_store import InMemoryConversationStore  # noqa: E402
from agent_runtime.adapters.outbound.tavily import TavilySearch  # noqa: E402
from agent_runtime.adapters.outbound.vllm import VLLMModel  # noqa: E402
from agent_runtime.application import ChatService, ChatSettings, ModelPort, SearchPort  # noqa: E402
from agent_runtime.config import Settings  # noqa: E402
from agent_runtime.domain import (  # noqa: E402
    GenerationParams,
    InferenceRequest,
    InferenceResult,
    SearchResult,
)
from agent_runtime.domain.client_tools import (  # noqa: E402
    ClientTools,
    MusicState,
    MusicTool,
    PlayingTrack,
    Station,
)
from evals import scoring  # noqa: E402

EVALS_DIR = Path(__file__).resolve().parent
DATASET = EVALS_DIR / "dataset.jsonl"
STATIONS_FILE = EVALS_DIR.parents[1] / "discord_bot" / "stations.toml"
EXPERIMENT = "tatarbot-agent"
DEFAULT_TRACKING_URI = f"sqlite:///{EVALS_DIR / 'mlflow.db'}"
# Each case is its own conversation, so a run never mixes histories.
CONVERSATION_PREFIX = "eval:"


# ---- ports wrapped for tracing


class TracedModel(ModelPort):
    """The real model, with one MLflow span per call (prompt, tools offered, reply)."""

    def __init__(self, inner: ModelPort):
        self._inner = inner

    async def generate(self, request: InferenceRequest) -> InferenceResult:
        with mlflow.start_span("model.generate", span_type=SpanType.CHAT_MODEL) as span:
            span.set_inputs(
                {
                    "messages": [
                        {"role": m.role.value, "content": m.content} for m in request.messages
                    ],
                    "tools": [t.name for t in request.tools],
                    "max_tokens": request.params.max_tokens,
                }
            )
            result = await self._inner.generate(request)
            span.set_outputs(
                {
                    "content": result.content,
                    "tool_calls": [
                        {"name": c.name, "arguments": c.arguments} for c in result.tool_calls
                    ],
                    "finish_reason": result.finish_reason,
                }
            )
            return result

    async def is_ready(self) -> bool:
        return await self._inner.is_ready()

    async def aclose(self) -> None:
        await self._inner.aclose()


class CannedSearch(SearchPort):
    """Search without the network: the same neutral placeholder results for any query. The
    eval scores whether and what the agent searches, not what the web says."""

    async def search(self, query: str, max_results: int) -> list[SearchResult]:
        return [
            SearchResult(
                title=f"{query} ({i})",
                url=f"https://example.com/eval/{i}",
                snippet=f"Placeholder search result {i} for {query!r}. Eval data, not real.",
            )
            for i in range(1, min(max_results, 3) + 1)
        ]


class TracedSearch(SearchPort):
    def __init__(self, inner: SearchPort):
        self._inner = inner
        self.queries: list[str] = []

    async def search(self, query: str, max_results: int) -> list[SearchResult]:
        self.queries.append(query)
        with mlflow.start_span("web_search", span_type=SpanType.TOOL) as span:
            span.set_inputs({"query": query, "max_results": max_results})
            results = await self._inner.search(query, max_results)
            span.set_outputs([{"title": r.title, "url": r.url} for r in results])
            return results

    async def aclose(self) -> None:
        await self._inner.aclose()


# ---- one turn


def load_stations() -> tuple[Station, ...]:
    with STATIONS_FILE.open("rb") as f:
        data = tomllib.load(f)
    return tuple(Station(s["name"], s["title"]) for s in data["station"])


def client_tools(
    stations: tuple[Station, ...],
    music_state: dict | None,
    library_hints: list[str] | None,
) -> ClientTools:
    state = None
    if music_state:
        state = MusicState(
            channel=music_state.get("channel"),
            queue=tuple(PlayingTrack(**t) for t in music_state.get("queue", ())),
        )
    return ClientTools(
        MusicTool(
            stations=stations,
            state=state,
            library=library_hints is not None,
            library_hints=tuple(library_hints or ()),
        )
    )


class Harness:
    def __init__(self, settings: Settings, live_search: bool):
        self.settings = settings
        self.live_search = live_search
        self.stations = load_stations()

    def chat_settings(self) -> ChatSettings:
        s = self.settings
        return ChatSettings(
            system_prompt=s.system_prompt,
            max_history_messages=s.max_history_messages,
            generation=GenerationParams(
                max_tokens=s.model_max_tokens, temperature=s.model_temperature
            ),
            guardrails_enabled=s.guardrails_enabled,
            max_searches=s.search_max_searches,
            results_per_search=s.search_max_results,
            turn_timeout_seconds=s.turn_timeout_seconds,
        )

    async def turn(
        self,
        case_id: str,
        message: str,
        music_state: dict | None,
        library_hints: list[str] | None,
    ) -> dict:
        s = self.settings
        # Built per turn: MLflow runs cases on worker threads, each with its own event loop,
        # and an httpx client must not cross loops.
        model = TracedModel(
            VLLMModel(
                s.model_base_url,
                s.model_name,
                api_key=s.model_api_key,
                timeout_seconds=s.model_timeout_seconds,
                enable_thinking=s.model_enable_thinking,
            )
        )
        inner = (
            TavilySearch(s.tavily_api_key, timeout_seconds=s.search_timeout_seconds)
            if self.live_search
            else CannedSearch()
        )
        search = TracedSearch(inner)
        service = ChatService(model, InMemoryConversationStore(), self.chat_settings(), search)

        # Keep the agent's own record of the turn (decisions, searches, actions), which
        # ChatService does not return. Eval only: production code is unchanged.
        agent = service._agent
        captured = {}
        original_run = agent.run

        async def run_and_capture(*args, **kwargs):
            captured["result"] = await original_run(*args, **kwargs)
            return captured["result"]

        agent.run = run_and_capture

        tools = client_tools(self.stations, music_state, library_hints)
        started = time.perf_counter()
        reply, error = "", None
        try:
            reply = (await service.reply(CONVERSATION_PREFIX + case_id, message, tools)).message
        except Exception as e:  # scored as a failed turn, not a crashed run
            error = f"{type(e).__name__}: {e}"
        finally:
            await model.aclose()
            await search.aclose()
        latency = time.perf_counter() - started

        result = captured.get("result")
        steps = result.steps if result else ()
        actions = result.actions if result else ()
        return {
            "reply": reply,
            "error": error,
            "decision": steps[0].decision.value if steps else None,
            "decisions": [st.decision.value for st in steps],
            "thoughts": [st.thought for st in steps],
            "searches": result.searches if result else 0,
            "search_queries": search.queries,
            "actions": [
                {"action": a.action.value, "station": a.station, "track": a.track} for a in actions
            ],
            "model_calls": result.model_calls if result else 0,
            "latency_s": round(latency, 3),
        }


# ---- MLflow scorers (thin wrappers over scoring.py)


@scorer
def turn_ok(outputs):
    return scoring.turn_ok(outputs)


@scorer
def decision_correct(outputs, expectations):
    return scoring.decision_correct(outputs, expectations)


@scorer
def behavior_correct(outputs, expectations):
    return scoring.behavior_correct(outputs, expectations)


@scorer
def music_args_correct(outputs, expectations):
    return scoring.music_args_correct(outputs, expectations)


@scorer
def format_ok(outputs):
    return scoring.format_ok(outputs)


@scorer
def language_ok(outputs, expectations):
    return scoring.language_ok(outputs, expectations)


@scorer
def no_leak(outputs):
    return scoring.no_leak(outputs)


@scorer(aggregations=["mean", "p90", "max"])
def latency_s(outputs):
    return outputs["latency_s"]


@scorer(aggregations=["mean", "max"])
def model_calls(outputs):
    return outputs["model_calls"]


@scorer(aggregations=["mean", "max"])
def searches(outputs):
    return outputs["searches"]


SCORERS = [
    turn_ok,
    decision_correct,
    behavior_correct,
    music_args_correct,
    format_ok,
    language_ok,
    no_leak,
    latency_s,
    model_calls,
    searches,
]
# Pass/fail checks, also reported per category.
CHECKS = {
    "turn_ok": lambda o, e: scoring.turn_ok(o),
    "decision_correct": scoring.decision_correct,
    "behavior_correct": scoring.behavior_correct,
    "music_args_correct": scoring.music_args_correct,
    "format_ok": lambda o, e: scoring.format_ok(o),
    "language_ok": scoring.language_ok,
    "no_leak": lambda o, e: scoring.no_leak(o),
}


# ---- run


def load_cases(path: Path, category: str | None, limit: int | None) -> list[dict]:
    cases = [json.loads(line) for line in path.read_text().splitlines() if line.strip()]
    if category:
        cases = [c for c in cases if c["category"] == category]
    return cases[:limit] if limit else cases


def git_commit() -> str:
    try:
        sha = subprocess.run(
            ["git", "rev-parse", "--short", "HEAD"], capture_output=True, text=True, check=True
        ).stdout.strip()
        dirty = subprocess.run(["git", "status", "--porcelain"], capture_output=True, text=True)
        return sha + ("-dirty" if dirty.stdout.strip() else "")
    except (OSError, subprocess.CalledProcessError):
        return "unknown"


def sha(text: str) -> str:
    return hashlib.sha256(text.encode()).hexdigest()[:12]


def run_params(settings: Settings, args, dataset: Path, cases: list[dict]) -> dict:
    return {
        "model_name": settings.model_name,
        "model_host": urlsplit(settings.model_base_url).hostname,
        "model_temperature": settings.model_temperature,
        "model_max_tokens": settings.model_max_tokens,
        "model_enable_thinking": settings.model_enable_thinking,
        "max_searches": settings.search_max_searches,
        "search": "tavily" if args.live_search else "canned",
        "system_prompt_sha": sha(settings.system_prompt),
        "dataset": dataset.name,
        "dataset_sha": sha(dataset.read_text()),
        "cases": len(cases),
        "category": args.category or "all",
        "git_commit": git_commit(),
    }


def summarize(cases: list[dict], outputs: dict[str, dict]) -> tuple[dict, list[dict]]:
    """Per-category pass rates (metric name -> value) and the failed checks, per case."""
    tally: dict[tuple[str, str], list[bool]] = defaultdict(list)
    failures = []
    for case in cases:
        out = outputs.get(case["id"])
        if out is None:
            continue
        failed = []
        for name, check in CHECKS.items():
            value = check(out, case["expectations"])
            if value is None:
                continue
            tally[(case["category"], name)].append(value)
            if not value:
                failed.append(name)
        if failed:
            failures.append(
                {
                    "case_id": case["id"],
                    "category": case["category"],
                    "message": case["inputs"]["message"],
                    "failed": ", ".join(failed),
                    "expected": json.dumps(case["expectations"], ensure_ascii=False),
                    "decisions": " > ".join(out["decisions"]) or "-",
                    "actions": json.dumps(out["actions"], ensure_ascii=False),
                    "search_queries": " | ".join(out["search_queries"]),
                    "reply": out["reply"][:300],
                    "error": out["error"] or "",
                }
            )
    metrics = {
        f"{category}/{name}": sum(values) / len(values)
        for (category, name), values in sorted(tally.items())
    }
    return metrics, failures


def print_report(metrics: dict, overall: dict, failures: list[dict]) -> None:
    categories = sorted({k.split("/")[0] for k in metrics})
    names = list(CHECKS)
    width = max(len(c) for c in categories) if categories else 10
    print("\nPass rate by category (- = not applicable)")
    print(" " * width + "  " + "  ".join(f"{n[:12]:>12}" for n in names))
    for c in categories:
        cells = [metrics.get(f"{c}/{n}") for n in names]
        row = "  ".join(f"{'-' if v is None else f'{v:.0%}':>12}" for v in cells)
        print(f"{c:<{width}}  {row}")
    print("\nOverall")
    for key in sorted(overall):
        print(f"  {key:<28} {overall[key]:.3f}")
    print(f"\n{len(failures)} case(s) with a failed check")
    for f in failures:
        print(f"  {f['case_id']:<12} {f['failed']:<40} decisions={f['decisions']}")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--dataset", type=Path, default=DATASET)
    parser.add_argument("--category", help="only the cases of this category")
    parser.add_argument("--limit", type=int, help="only the first N cases")
    parser.add_argument("--live-search", action="store_true", help="use Tavily, not canned")
    parser.add_argument("--workers", type=int, default=4, help="cases run in parallel")
    parser.add_argument("--run-name", help="MLflow run name (default: model name)")
    parser.add_argument("--experiment", default=EXPERIMENT)
    args = parser.parse_args(argv)

    settings = Settings.from_env()
    if args.live_search and not settings.tavily_api_key:
        parser.error("--live-search needs TAVILY_API_KEY")
    cases = load_cases(args.dataset, args.category, args.limit)
    if not cases:
        parser.error("no cases selected")

    os.environ["MLFLOW_GENAI_EVAL_MAX_WORKERS"] = str(args.workers)
    mlflow.set_tracking_uri(os.environ.get("MLFLOW_TRACKING_URI", DEFAULT_TRACKING_URI))
    if mlflow.get_experiment_by_name(args.experiment) is None:
        mlflow.create_experiment(
            args.experiment, artifact_location=(EVALS_DIR / "mlartifacts").as_uri()
        )
    mlflow.set_experiment(args.experiment)

    harness = Harness(settings, args.live_search)
    outputs: dict[str, dict] = {}

    # The root span of each case's trace; the model and search spans nest under it.
    @mlflow.trace(name="chat_turn", span_type=SpanType.AGENT)
    def predict_fn(case_id, category, message, music_state=None, library_hints=None):
        out = asyncio.run(harness.turn(case_id, message, music_state, library_hints))
        outputs[case_id] = out
        return out

    data = [
        {
            "inputs": {"case_id": c["id"], "category": c["category"], **c["inputs"]},
            "expectations": c["expectations"],
        }
        for c in cases
    ]
    params = run_params(settings, args, args.dataset, cases)
    with mlflow.start_run(run_name=args.run_name or settings.model_name) as run:
        mlflow.log_params(params)
        result = mlflow.genai.evaluate(data=data, predict_fn=predict_fn, scorers=SCORERS)
        metrics, failures = summarize(cases, outputs)
        mlflow.log_metrics(metrics)
        if failures:
            mlflow.log_table(
                {k: [f[k] for f in failures] for k in failures[0]}, artifact_file="failures.json"
            )

    overall = {k: float(v) for k, v in result.metrics.items()}
    print_report(metrics, overall, failures)
    print(f"\nMLflow run {run.info.run_id} in experiment {args.experiment!r}.")
    print("Open it with: make -C agent_runtime eval-ui")
    return 0


if __name__ == "__main__":
    sys.exit(main())
