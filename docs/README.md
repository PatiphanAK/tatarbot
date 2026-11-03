# Documentation map

Where each topic lives. System-wide documents are in this directory; anything about one
module sits next to its code.

## System

| Topic | Document |
|---|---|
| Overview, architecture diagram, how playback, uploads, search and the agent work, and why | [README.md](../README.md#how-it-works) |
| API contracts and integration flows (bot ↔ runtime, uploads) | [architecture/api.md](architecture/api.md) |
| Database schemas, data lifecycle, migrations, connections | [architecture/database.md](architecture/database.md) |
| Knowledge base (RAG): pipeline, embedding models, tuning, persistent store, ingestion, hybrid retrieval | [architecture/rag.md](architecture/rag.md) |
| Cloud inference audit and design (Phase 0 record) | [architecture/cloud-inference.md](architecture/cloud-inference.md) |
| Security model and access control | [security.md](security.md) |
| Development setup, local services, test suites, conventions | [development.md](development.md) |
| Troubleshooting by symptom and log line | [operations/troubleshooting.md](operations/troubleshooting.md) |
| Infrastructure, deployment, cost, CI/CD (GCP) | [infra/gcp/README.md](../infra/gcp/README.md) |
| Previous AWS deployment (live until the migration) | [infra/aws/README.md](../infra/aws/README.md) |

## Decisions

| ADR | Decision |
|---|---|
| [0001](adr/0001-inference-provider.md) | Inference on Runpod Serverless vLLM behind an OpenAI-compatible API (Proposed) |
| [0002](adr/0002-persistent-rag-store.md) | pgvector + trigram hybrid retrieval in the shared Postgres; S3 presigned uploads with SQS-driven ingestion (Accepted, not deployed) |
| [0003](adr/0003-deploy-on-gcp.md) | Deploy on one GCE e2-micro VM in asia-southeast1 with COS and systemd, replacing AWS ECS (Accepted, not applied) |

New ADRs: next number, `Status / Date / Details` header, then Context, Decision,
Alternatives considered, Consequences, Revisit when.

## Modules

| Module | README | More |
|---|---|---|
| Discord bot | [discord_bot/README.md](../discord_bot/README.md): layout, configuration, commands, operations | [discord_bot/docs/](../discord_bot/docs/README.md): outbound adapters (voice, ICY, sqlx library) |
| Agent runtime | [agent_runtime/README.md](../agent_runtime/README.md): architecture, API, configuration | [domain](../agent_runtime/src/agent_runtime/domain/README.md), [application](../agent_runtime/src/agent_runtime/application/README.md), [adapters](../agent_runtime/src/agent_runtime/adapters/README.md), [db](../agent_runtime/src/agent_runtime/db/README.md), [tests](../agent_runtime/tests/README.md) |
| Model inference | [model-inference/README.md](../model-inference/README.md): local vLLM, Runpod template, smoke test | |
| Infrastructure | [infra/gcp/README.md](../infra/gcp/README.md): Terraform, operations, migration from AWS, CI/CD | [infra/aws/README.md](../infra/aws/README.md) (until teardown) |
| Integration tests | [integration/README.md](../integration/README.md) | |

## Keeping docs current

* Change the doc in the same commit as the code it describes.
* Reference files and symbols instead of copying code; the code and SQL migrations are the
  source of truth for signatures and schemas.
* Mark anything not built as planned, and anything not measured as unmeasured.
