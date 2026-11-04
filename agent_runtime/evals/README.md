# Agent eval (MLflow)

The unit tests in `tests/` use a fake model: they check the code, not the model. This eval
runs the **real model** (the one in `agent_runtime/.env`) through the real `ChatService` and
ReAct agent, scores each turn, and logs the run to MLflow so runs can be compared when the
model, the prompts or the agent change.

```sh
make -C agent_runtime eval                                   # all cases (~1 min, 4 at a time)
make -C agent_runtime eval ARGS="--category music-play"      # one category
make -C agent_runtime eval ARGS="--limit 5 --run-name try"   # quick check
make -C agent_runtime eval ARGS="--live-search"              # Tavily instead of canned search
make -C agent_runtime eval-ui                                # http://127.0.0.1:5000
```

MLflow is in the `eval` dependency group (`uv run --group eval`): it is not installed by
`make install`, CI or the Docker image. Runs are stored locally in `evals/mlflow.db` and
`evals/mlartifacts/` (git-ignored). Set `MLFLOW_TRACKING_URI` to log to a shared server.

**It costs money and is not deterministic.** Every case calls the model 2-8 times. Two runs of
the same setup can differ by a few cases, so compare runs, not single cases, and rerun before
trusting a small difference.

## What a case is

One line of `dataset.jsonl`: a single user turn in a fresh conversation, with the bot's music
tool offered (as the bot does in a server) and web search enabled.

```json
{"id": "play-en-1", "category": "music-play",
 "inputs": {"message": "play some jazz"},
 "expectations": {"decision": "music", "language": "en", "action": "play", "station": "jazz"}}
```

* `inputs`: `message`, plus optional `music_state` (what the bot reports is playing) and
  `library_hints` (titles in the server's uploaded library; enables `play_track`).
* `expectations`: see the docstring of `scoring.py`. `decision` may be a list when more than
  one choice is fine.

59 cases, Thai and English: small talk, stable knowledge, current information, playing a
station, music control, the uploaded library, and questions *about* music that must not start
playback.

Search is canned by default (placeholder results for any query), so the eval scores whether
and what the agent searches, not what the web says. With canned results the model often
searches again, so `searches`, `model_calls` and latency of search cases are higher than with
`--live-search`.

## Checks

| Check | Passes when |
|---|---|
| `turn_ok` | the turn finished (no timeout, model error or empty reply) |
| `decision_correct` | the first reasoning step chose an acceptable `Next:` (search, answer, music) |
| `behavior_correct` | the turn as a whole searched, answered or took a music action, as expected |
| `music_args_correct` | music cases: exactly one action, with the right action, station and track |
| `format_ok` | every reasoning step ended with a parseable `Next:` line |
| `language_ok` | the reply is in the user's language (replies without letters don't count) |
| `no_leak` | the reply has no agent scaffolding (`Thought:`, `Next:`, tool-call markup) |
| `latency_s`, `model_calls`, `searches` | recorded, with mean / p90 / max |

A check that does not apply to a case (e.g. `music_args_correct` on a search case) is left
out of its mean. The rules are plain functions in `scoring.py`, unit-tested in
`tests/evals_test/` without MLflow.

## What is logged

Per run: parameters (model, host, temperature, thinking, search mode, system-prompt and
dataset hashes, git commit), overall metrics, metrics per category
(`music-play/decision_correct`...), and `failures.json` (each failed case with its decisions,
actions, search queries and reply). Per case: a trace with every model call (prompt, tools
offered, reply, tool calls) and search, with the checks attached as assessments. In the UI:
experiment `tatarbot-agent` → a run → **Traces**; select runs → **Compare**.

## Baseline (2026-11-05, `qwen3.8-flash` on Alibaba Model Studio, canned search)

| Check | Score |
|---|---|
| turn_ok | 1.00 |
| decision_correct | 0.93 |
| behavior_correct | 0.98 |
| music_args_correct | 1.00 |
| format_ok | 0.92 |
| language_ok, no_leak | 1.00 |
| latency mean / p90 | 5.2 s / 10.8 s |

Findings from the first runs (traces in MLflow):

1. **`Next:` on the same line as the thought.** The model often writes
   `... no external information is needed. Next: ANSWER` instead of putting it on its own
   line. `_NEXT_LINE` in `application/agent.py` only matches at the start of a line, so the
   step becomes `unclear`: every tool is offered and the turn usually still works, but the
   plan is lost. Most `decision_correct` and `format_ok` failures were this.
   **Fixed:** `_NEXT_INLINE` is a stricter fallback for a decision at the end of a line.
   Replaying the 325 reasoning replies recorded in the first runs through both parsers:
   unparseable replies fell from 28 (8.6 %) to 4 (1.2 %; the 4 are empty replies where the
   model called a tool it wasn't offered), no decision the old parser read changed, and all
   10 recovered first-step decisions match the expected one. Not yet confirmed with a live
   rerun: the model's free quota ran out (HTTP 403 `insufficient_quota`).
2. **Claims a music action without calling the tool.** Once, for "หยุดเพลงได้แล้ว" (stop
   the music), the model decided MUSIC but replied "หยุดเพลงแล้วครับ" (stopped) with no
   `play_music` call: nothing happened, but the user was told it did. The agent has no retry
   for a MUSIC decision without a call.
3. **Fake tool results.** Once, offered `web_search`, the model wrote a made-up
   `[Tool Result]` block with search results as text instead of calling the tool.

Findings 2 and 3 appeared in one run out of three; the eval catches them as
`behavior_correct` failures.

## Adding cases

Add lines to `dataset.jsonl` (unique `id`; `tests/evals_test` validates the file). Prefer
cases that failed in real use. Keep the categories balanced; per-category metrics show
where a change helps or hurts.

Not covered yet: the knowledge base (RAG retrieval), multi-turn conversations, and answer
quality judged by a model (`mlflow.genai` LLM judges).
