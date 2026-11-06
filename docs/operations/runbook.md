# Runbook: start, stop, health, model provider

Commands for running TatarBot locally and on GCP, checking it, and switching the model
provider (Alibaba Qwen ↔ OpenAI). Run them from the repository root unless a `cd` says
otherwise. Architecture: [infra/gcp/README.md](../../infra/gcp/README.md); errors:
[troubleshooting.md](troubleshooting.md).

**Paid calls.** Every chat turn calls the model provider (2 calls for a plain answer, one
more per web search, at most `AGENT_MAX_MODEL_CALLS` = 10) and Tavily once per search.
`/ready` calls the provider's free `GET /models`. Health checks marked *free* make no
provider calls.

## What runs where

| | Local | GCP |
|---|---|---|
| Agent Runtime | `make -C agent_runtime run` on `127.0.0.1:8080` | Cloud Run `tatarbot-runtime` (private, scales to zero) |
| Discord bot | `make -C discord_bot run` | VM `tatarbot` (Docker, systemd `tatarbot-discord`) |
| Model | `agent_runtime/.env` `MODEL_*` | Terraform `model_provider`; keys in `tatarbot-model-api-key` (Alibaba) / `tatarbot-openai-api-key` |
| Schedule | while you run it | `vm_schedule` (now 18:00–24:00 Bangkok); `vm_schedule_paused = true` = 24/7 |

Never run the bot locally and on the VM with the same `DISCORD_TOKEN`: both would answer.

## Local

```sh
# Start (runtime, waits for /health, then the bot; Ctrl-C stops both)
make up

# Or separately, each in its own terminal
make -C agent_runtime run
make -C discord_bot run

# Stop: Ctrl-C in the terminal. A runtime left in the background:
pkill -f "uvicorn --factory agent_runtime.main:create_app"

# Health (free): process up
curl -s http://127.0.0.1:8080/health          # {"status":"ok"}
# Readiness: the model answers GET /models (free, but contacts the provider)
curl -s http://127.0.0.1:8080/ready           # {"status":"ready","model":true}

# Which provider is active: the first log line of the runtime
#   agent runtime started (model=gpt-4.1-mini, model_host=api.openai.com, thinking=omit, ...)

# One paid chat turn (1-2 model calls)
make -C agent_runtime smoke
```

## GCP

```sh
ZONE=asia-southeast1-b; REGION=asia-southeast1

# Status (free)
gcloud compute instances describe tatarbot --zone $ZONE --format='value(status)'   # RUNNING / TERMINATED
gcloud run services describe tatarbot-runtime --region $REGION \
  --format='value(status.latestReadyRevisionName,status.conditions[0].status)'

# Start / stop the bot (the runtime starts on demand and needs no start/stop)
gcloud compute instances start tatarbot --zone $ZONE
gcloud compute instances stop tatarbot --zone $ZONE

# Bot logs (IAP SSH)
gcloud compute ssh tatarbot --zone $ZONE --tunnel-through-iap \
  --command 'sudo systemctl is-active tatarbot-discord; sudo docker logs --tail 50 tatarbot-discord'

# Runtime: startup line (which provider) and the last turns' metrics
gcloud logging read 'resource.type="cloud_run_revision" AND resource.labels.service_name="tatarbot-runtime" AND textPayload:"agent runtime started"' \
  --limit 1 --freshness 1d --format='value(timestamp,textPayload)'
gcloud logging read 'resource.type="cloud_run_revision" AND resource.labels.service_name="tatarbot-runtime" AND (textPayload:"chat turn completed" OR textPayload:"agent turn failed" OR textPayload:"model returned HTTP")' \
  --limit 20 --freshness 1h --format='value(timestamp,textPayload)'
```

A healthy turn logs `chat turn completed ... searches=N model_calls=M input_tokens=...
output_tokens=... cost_usd=... latency_ms=...`. Failures log `agent turn failed ...
error=<type> model_calls=... failed_calls=...` and `model returned HTTP <status>`.

## Switching the model provider

The runtime talks to any OpenAI-compatible Chat Completions API. A provider is four
settings: base URL, model name, API key, and `MODEL_ENABLE_THINKING` (`false` for Qwen3,
`omit` for OpenAI, which rejects Qwen's `chat_template_kwargs` with HTTP 400).

| Setting | OpenAI (temporary) | Alibaba Model Studio (Qwen) |
|---|---|---|
| `MODEL_BASE_URL` | `https://api.openai.com/v1` | `https://ws-8gxzcgr1p5rp24wa.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1` |
| `MODEL_NAME` | `gpt-4.1-mini` | `qwen3.8-flash` |
| `MODEL_ENABLE_THINKING` | `omit` | `false` |
| `MODEL_TIMEOUT_SECONDS` | `30` | `100` |
| Key | OpenAI project key | Model Studio key |

Only `gpt-4.1*` / `gpt-4o*` models work as is: `gpt-5*` models reject `max_tokens`.

### Local

`agent_runtime/.env` has both providers; one block is commented out. Comment the active
block, uncomment the other, and restart the runtime (Ctrl-C, `make up`). Check the startup
line (`model=..., model_host=...`) and `curl -s http://127.0.0.1:8080/ready`.

### GCP (needs approval: changes the live service)

Terraform selects the provider with `model_provider` (`alibaba` | `openai`). Each provider
has its own key secret: `tatarbot-model-api-key` (Alibaba, unchanged) and
`tatarbot-openai-api-key` (created by `openai_enabled = true`). The runtime can read both;
only the active one is injected as `MODEL_API_KEY`. For `openai`, Terraform sets
`MODEL_BASE_URL=https://api.openai.com/v1`, `MODEL_NAME=gpt-4.1-mini`
(`openai_model_name`), `MODEL_ENABLE_THINKING=omit` and `MODEL_TIMEOUT_SECONDS=30`
(`openai_model_timeout_seconds`). `model_base_url` / `model_name` keep the Alibaba values.

Order matters: an image without `omit` support would refuse `MODEL_ENABLE_THINKING=omit`
(its revision would not start; the previous one keeps serving).

```sh
cd infra/gcp
```

1. **Phase 1, the secret** (2 resources added: the secret and the runtime's read access).
   Add `openai_enabled = true` to `terraform.tfvars`, then:

   ```sh
   terraform plan -out=openai-secret.tfplan    # expect: 2 to add, 0 to change, 0 to destroy
   terraform apply openai-secret.tfplan
   ```

2. **The key**, read from `agent_runtime/.env` without printing it:

   ```sh
   printf %s "$(sed -n 's/^MODEL_API_KEY=//p' ../../agent_runtime/.env)" \
     | gcloud secrets versions add tatarbot-openai-api-key --data-file=-
   gcloud secrets versions list tatarbot-openai-api-key   # one ENABLED version
   ```

   Check first that `.env`'s active `MODEL_API_KEY` is the OpenAI key (`sk-proj-...`).

3. **The image.** Merge the PR (after PR #25), then deploy the runtime from `main`:

   ```sh
   gh workflow run deploy-gcp-runtime.yml -f ref=main
   gh run watch "$(gh run list --workflow deploy-gcp-runtime.yml --limit 1 --json databaseId -q '.[0].databaseId')"
   ```

4. **Phase 2, switch and run 24/7.** In `terraform.tfvars` add `model_provider = "openai"`
   and `vm_schedule_paused = true`, then:

   ```sh
   terraform plan -out=openai-switch.tfplan    # expect: 0 to add, 2 to change, 0 to destroy
   terraform apply openai-switch.tfplan
   gcloud compute instances start tatarbot --zone asia-southeast1-b   # if it is stopped
   ```

   The Cloud Run change replaces 4 env vars (`MODEL_API_KEY` secret, `MODEL_BASE_URL`,
   `MODEL_NAME`, `MODEL_TIMEOUT_SECONDS`) and adds `MODEL_ENABLE_THINKING`; the VM change
   only detaches the schedule. Nothing is destroyed.

5. **Check.** The startup line shows `model=gpt-4.1-mini, model_host=api.openai.com,
   thinking=omit` (GCP section above); one chat in Discord answers; its `chat turn
   completed` line has `input_tokens` set.

Never apply an old saved plan (e.g. `fix.tfplan` from 2026-11-04): make a fresh one.

**Rollback to Alibaba** (once its quota works again): set `model_provider = "alibaba"`
(keep `openai_enabled = true`), plan (expect: Cloud Run updated in place only), apply. The
Alibaba secret was never changed. Faster, without Terraform: move traffic to the last
Alibaba revision (`gcloud run revisions list --service tatarbot-runtime --region
asia-southeast1`, then `gcloud run services update-traffic tatarbot-runtime --region
asia-southeast1 --to-revisions <rev>=100`); run `terraform apply` afterwards so the
configuration matches again, and `gcloud run services update-traffic tatarbot-runtime
--region asia-southeast1 --to-latest` to return traffic to the newest revision.

**Back to the daily schedule:** `vm_schedule_paused = false`, plan, apply.

## Running 24/7 (needs approval)

Nothing new is provisioned: the VM and Cloud Run already exist.

1. **VM always on:** `vm_schedule_paused = true` (step 4 above). This detaches the schedule
   and keeps its policy and custom role. Do not use `vm_schedule = null` for a temporary
   change: it deletes the custom role, and GCP keeps a deleted role's id reserved for
   weeks, so turning the schedule back on would fail.
2. **Runtime:** already available at any hour (scales to zero; the first message after ~15
   idle minutes waits for a cold start). For no cold starts during the demo:
   `runtime_min_instances = 1` (billed while idle), apply; set back to 0 afterwards.
3. **Restarts:** the bot's systemd unit restarts it on failure and after a reboot; Cloud
   Run replaces failed instances. Model calls are never retried: a failed turn fails once
   and the user can ask again.
4. **Costs:** model and Tavily calls scale with use. The GCP budget alert (`budget_amount`,
   THB) covers GCP only: set a monthly usage limit in the OpenAI dashboard as well.

## Failure guide

| Symptom (runtime log) | HTTP to the bot | Likely cause | Action |
|---|---|---|---|
| `model returned HTTP 429` ... `insufficient_quota` | 503 | Provider quota or billing | Fix billing, or switch provider |
| `model returned HTTP 429` (rate limit) | 503 | Too many requests | Wait; lower usage |
| `model returned HTTP 403` ... `free quota has been exhausted` | 502 | Alibaba free tier ended | Enable paid use, or switch to OpenAI |
| `model returned HTTP 400` ... `chat_template_kwargs` | 502 | OpenAI with `MODEL_ENABLE_THINKING` not `omit` | Set `omit` |
| `model returned HTTP 401` | 502 | Wrong key for that provider | Check the secret / `MODEL_API_KEY` |
| `agent turn failed ... error=ModelTimeoutError` | 504 | Slow provider or long turn | Check `MODEL_TIMEOUT_SECONDS`, `TURN_TIMEOUT_SECONDS` |
| `agent turn failed ... error=ModelCallLimitError` | 502 | Turn hit `AGENT_MAX_MODEL_CALLS` | Should not happen; report it |
| `ConfigError` at startup | runtime down | Invalid setting (the message names it) | Fix the variable |
