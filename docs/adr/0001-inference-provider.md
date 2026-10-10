# ADR 0001: Inference provider

- Status: Proposed
- Date: 2026-10-10
- Details: [Cloud inference: audit and design](../architecture/cloud-inference.md)

## Context

TatarBot is a Discord bot (`discord_bot/`, Rust) that forwards messages to an Agent Runtime
(`agent_runtime/`, Python). The runtime calls a model through `ModelPort`, implemented today
by `VLLMModel`, a non-streaming OpenAI Chat Completions client
(`agent_runtime/src/agent_runtime/adapters/outbound/vllm.py`). The model currently runs on
an ad-hoc GPU host reached through a temporary tunnel, which is not reproducible.

The goal is a reproducible, low-cost cloud deployment for a portfolio project with a
low-volume workload: AWS for the always-on services and operations, a GPU provider for
inference, and no change to the bot or the runtime's behaviour.

## Decision

1. **Inference runs on Runpod Serverless using the official vLLM worker**, scaled to zero,
   with a small hard cap on workers. The runtime reaches it through Runpod's
   OpenAI-compatible route (`https://api.runpod.ai/v2/<ENDPOINT_ID>/openai/v1`) with the
   Runpod API key as `MODEL_API_KEY`. This is subject to the verification items in the design
   doc §4.2; if they fail, fall back to the alternatives below without code changes.
2. **The existing `ModelPort` + OpenAI-compatible adapter is the provider-independent
   interface.** No new abstraction, no Runpod SDK, no custom worker handler.
3. **AWS runs the bot and the runtime as two independent ECS Fargate services**
   (`tatarbot-discord`, `tatarbot-runtime`, one task each). The bot reaches the runtime
   through Cloud Map private DNS. The runtime accepts traffic only from the bot's security
   group, and the bot accepts none. Secrets come from SSM Parameter Store, images from one
   ECR repository per service, logs go to CloudWatch, and a Budgets alert guards cost. AWS
   does not host GPUs. *(Revised 2026-10-10: originally one task with both containers;
   split so each service builds and deploys independently.)*
4. **Model:** a Qwen3 instruct model, starting around 4B parameters, non-thinking variant
   preferred, `MAX_MODEL_LEN` 16384, weights cached rather than downloaded on every cold
   start. Final choice after measuring cold start and price.

## Alternatives considered

| Option | Why not (now) |
|---|---|
| Dedicated GPU VM (Runpod Pod or AWS g-class EC2) running vLLM | Pays for idle hours; we would have to secure the vLLM port ourselves; AWS GPU quota may need a request. Remains the fallback if cold starts are unacceptable or serverless behaviour fails verification. Same adapter works. |
| AWS SageMaker / Bedrock custom model import | More AWS-native, but heavier setup, different invocation APIs (would need a new adapter or a translation layer), and higher minimum cost for a low-volume bot. |
| Hosted inference API (OpenAI-compatible third-party) | Cheapest to operate, but does not demonstrate serving an open-weight model, which is part of the portfolio goal. Kept as an emergency fallback (config change only). |
| Runpod Serverless with a custom handler / native `/runsync` API | Requires a new `ModelPort` adapter and job polling; no benefit while the OpenAI route works. |
| Kubernetes + KServe/vLLM, API gateway, model registry | Disproportionate for one model and low traffic. Explicitly out of scope. |
| Keep the tunnel | Not reproducible, URL changes, no access control beyond an optional key. |

## Consequences

Positive:
- No code change to the bot or the runtime; switching provider is a configuration change.
- Near-zero GPU cost when idle; no GPU host to patch.
- No public ingress on the AWS side; the only internet-facing inference surface is Runpod's
  authenticated endpoint.

Negative / to manage:
- Cold starts: the first message after idle may be slow or time out (current limits: 100 s
  runtime, 120 s bot). Must be measured; timeouts may need raising together.
- The runtime's `/ready` (which calls `GET /models`) is not a safe periodic probe against a
  scale-to-zero endpoint; container health checks use `/health`.
- Inference security depends on one API key held in SSM; it must be endpoint-scoped if
  Runpod supports that, and spending must be capped by max workers and alerts.
- Single replica: in-memory history is lost on every deploy, accepted for now.
- Vendor-specific behaviours (sync wait limits, `chat_template_kwargs` passthrough, idle
  `max_workers` reduction) are unverified until the Phase 2 smoke tests run.

## Revisit when

- Measured cold starts make the bot unusable after idle → consider a dedicated VM on a
  schedule, or one active worker during demo hours.
- Traffic grows to where a GPU would be busy most of the time → dedicated VM is cheaper.
- Streaming, tools or multiple models are needed → reassess the adapter and provider.
