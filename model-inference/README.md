# Model inference (vLLM)

The Agent Runtime calls any OpenAI-compatible Chat Completions endpoint
(`agent_runtime/src/agent_runtime/adapters/outbound/vllm.py`). This directory has the two
ways TatarBot serves that endpoint with vLLM, and a smoke test that calls it exactly as the
runtime does. Neither needs a code change in the runtime: only `MODEL_BASE_URL`,
`MODEL_NAME` and `MODEL_API_KEY` differ.

| | Local (`local/`) | Production (`runpod/`) |
|---|---|---|
| Runs on | Your NVIDIA GPU, Docker Compose, `vllm/vllm-openai` | Runpod Serverless, official `worker-vllm`, scale to zero |
| Model | `Qwen/Qwen3-1.7B` (fits 6 GB) | `Qwen/Qwen3-4B-Instruct-2507` |
| Served name | `tatarbot-chat` | `tatarbot-chat` |
| `MODEL_BASE_URL` | `http://127.0.0.1:8000/v1` | `https://api.runpod.ai/v2/<ENDPOINT_ID>/openai/v1` |
| `MODEL_API_KEY` | `VLLM_API_KEY` from `local/.env` | Runpod API key (endpoint-scoped if possible) |
| Context | 8192 (VRAM) | 16384 |

Design and reasoning: [ADR 0001](../docs/adr/0001-inference-provider.md),
[cloud inference design](../docs/architecture/cloud-inference.md) §3.3, §3.5, §5 Phase 2.

```sh
make -C model-inference help
```

## Smoke test

`scripts/smoke.py` (standard library only) reads the runtime's variables and checks:

1. `POST /chat/completions` with the runtime's exact body (system prompt, `stream: false`,
   `max_tokens`, `chat_template_kwargs.enable_thinking`) → 200 with non-empty text. Run
   twice and timed: the first includes any cold start, the second is warm.
2. `GET /models` lists `MODEL_NAME` (what the runtime's `/ready` checks, with 3 s). Warns
   if it takes longer than that.
3. The same chat request without the key → 401/403.
4. A request with the runtime's `web_search` tool → 200 (tool calling is enabled), and
   whether the model chose to call it (a warning if not; small models search less often).

Each request may take up to `SMOKE_TIMEOUT_SECONDS` (300) so a cold start is measured
rather than cut off at the runtime's 100 s. It exits non-zero on the first failure and
prints only a short prefix of error bodies, never the key.

```sh
make -C model-inference smoke-local   # local server
make -C model-inference smoke         # whatever agent_runtime/.env points at (e.g. Runpod)
```

Then check the full path with the runtime: `make -C agent_runtime run` and
`make -C agent_runtime smoke`.

## Local vLLM

Needs an NVIDIA GPU, its driver, and the
[NVIDIA Container Toolkit](https://docs.nvidia.com/datacenter/cloud-native/container-toolkit/latest/install-guide.html)
(`docker info` should list an `nvidia` runtime).

```sh
make -C model-inference env      # local/.env with a generated VLLM_API_KEY
make -C model-inference serve    # first start downloads the weights into a Docker volume
make -C model-inference smoke-local
```

Point the runtime at it in `agent_runtime/.env`:

```sh
MODEL_BASE_URL=http://127.0.0.1:8000/v1
MODEL_NAME=tatarbot-chat
MODEL_API_KEY=<VLLM_API_KEY from model-inference/local/.env>
```

`make stop` keeps the weights, `make clean` deletes them, `make logs` follows the server.
The port is published on `127.0.0.1` only, which serves a runtime started with `make up`.
For the Compose runtime (`MODEL_BASE_URL=http://host.docker.internal:8000/v1`), set
`VLLM_BIND` to the Docker bridge address (usually `172.17.0.1`) rather than `0.0.0.0`.

Out of memory at start? Lower `MAX_MODEL_LEN` or `GPU_MEMORY_UTILIZATION`, add
`VLLM_EXTRA_ARGS=--enforce-eager`, or pick a smaller model (`Qwen/Qwen3-0.6B`). If you
lower `MAX_MODEL_LEN`, lower `MAX_HISTORY_MESSAGES` in the runtime as well.

## Runpod Serverless endpoint

**Status: prepared, not created.** Creating it bills per second of worker time.

1. Runpod console → Serverless → New Endpoint → vLLM worker (`runpod/worker-v1-vllm`,
   pin a release tag rather than `latest`).
2. Environment variables: copy `runpod/endpoint.env.example`. `HF_TOKEN` only for gated
   models, as a Runpod secret.
3. Model caching: set the endpoint's model to `Qwen/Qwen3-4B-Instruct-2507` so weights are
   cached on the host instead of downloaded on each cold start. Prefer this over a network
   volume, which pins the endpoint to one datacenter.
4. Scaling and limits:

   | Setting | Value | Why |
   |---|---|---|
   | GPU | 16 GB class or larger (e.g. A4000/A4500/L4); add a 24 GB fallback | ~8 GB BF16 weights + KV cache for 16k context |
   | Active (min) workers | 0 | Scale to zero; nothing bills while idle |
   | Max workers | 1 (2 at most) | Hard spend cap; one bot needs no more |
   | Idle timeout | 60–120 s | Keeps a conversation warm; tune against measured cold start |
   | Execution timeout | 150 s | Above the runtime's `MODEL_TIMEOUT_SECONDS` (100 s); bounds abandoned jobs |
   | FlashBoot | On | Faster restarts of recently used workers |

5. API key: Settings → API Keys, restricted to this endpoint if Runpod allows it (never the
   account-wide key). Store it only in `agent_runtime/.env` locally and in SSM
   (`MODEL_API_KEY`) for ECS.
6. Smoke test before pointing AWS at it:

   ```sh
   # agent_runtime/.env
   MODEL_BASE_URL=https://api.runpod.ai/v2/<ENDPOINT_ID>/openai/v1
   MODEL_NAME=tatarbot-chat
   MODEL_API_KEY=<runpod key>

   make -C model-inference smoke
   ```

   Record cold-start and warm latency in the design doc (§5 Phase 2). If the cold reply
   takes more than ~90 s, raise `MODEL_TIMEOUT_SECONDS` and the bot's `AGENT_TIMEOUT_SECS`
   together (runtime < bot), and the execution timeout above them.

Things the smoke test settles (design doc §4.2): whether `chat_template_kwargs` passes
through (a 4xx on the chat check; with the non-thinking model, unset
`MODEL_ENABLE_THINKING` in the runtime), and whether `/models` answers within 3 s without a
running worker (if not, the runtime's `/ready` stays a manual check; ECS uses `/health`).

## Changing the model

Change `MODEL` (local) or `MODEL_NAME` (Runpod) and keep the served name `tatarbot-chat`;
the runtime and AWS need no change. Check the model's licence, that it fits the GPU at the
chosen `MAX_MODEL_LEN`, and re-run the smoke test.
