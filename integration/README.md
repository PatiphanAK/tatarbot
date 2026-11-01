# integration

Black-box tests of the agent runtime **container image** against a mock OpenAI-compatible
model. They check the HTTP contract the bot relies on, as the bot would see it, over a
Compose network. No Discord token, GPU or cloud credentials needed; nothing is published
to the host.

```sh
make integration        # from the repo root; or ./integration/run.sh
```

CI runs it on an arm64 runner (`ci-runtime.yml`, the deployed architecture).

## Pieces

| File | Role |
|---|---|
| `compose.yaml` | `mock_model`, `agent_runtime` (built from `../agent_runtime`), `tester` (profile `test`) |
| `mock_openai.py` | Standard-library OpenAI Chat Completions server: `GET /v1/models`, `POST /v1/chat/completions`. Echoes the last user message with a turn counter inside a `<think>` block the runtime must strip. Magic messages: `__fail_500__`, `__bad_json__`, `__slow__`, `__play_lofi__` (a `play_music` tool call when the tool is offered) |
| `test_stack.py` | Standard-library checks, run inside the `tester` container: `up` with the model running, `down` with it stopped |
| `run.sh` | Builds and starts the stack, runs `up`, stops the mock, runs `down`, prints logs on failure, always tears down |

## What is checked

* With the model: `/health` and `/ready`; 401 without or with a wrong key; replies come
  from the model with reasoning stripped; history is sent on later turns and
  conversations are isolated; 422 for a blank message; model 5xx → 503, a non-OpenAI body →
  502, a timeout → 504, with no upstream detail leaked; a music request returns a
  `play_music` action instead of text.
* Without the model: `/health` stays 200, `/ready` is 503, chat is 503 with no detail leaked.

## Not covered

Web search, the knowledge base (the mock has no `/embeddings` route), the Postgres store
and uploads. Those are covered by the runtime's own tests (`agent_runtime/tests/`).

## Extending

Add a magic message to `mock_openai.py` for a new upstream behaviour and a `check(...)` in
`test_stack.py`. Keep both standard-library only: they run in a stock `python:3.12-slim`
image with this directory mounted read-only.
