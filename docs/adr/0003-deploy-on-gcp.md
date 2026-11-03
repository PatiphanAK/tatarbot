# ADR 0003: Deploy on one GCE VM in Singapore, replacing AWS ECS

- Status: Accepted (Terraform on `feat/gcp-deploy`; `plan` only, not applied)
- Date: 2026-11-03
- Details: [infra/gcp/README.md](../../infra/gcp/README.md),
  [previous AWS design](../../infra/aws/README.md)

## Context

TatarBot runs two independent containers: the Rust Discord bot (gateway WebSocket plus UDP
voice, outbound only) and the Python Agent Runtime (HTTP, called only by the bot). State
lives outside them: Supabase Postgres (ap-southeast-1) for library metadata, object storage
for library audio, and conversation history in the runtime's memory.

The AWS stack (ECS on one t4g.micro, `infra/aws`) was applied on 2026-10-19 with a
3-hours-a-day schedule to stay near $3/month. 24/7 on AWS costs about $15. The decision is
to move the deployment to GCP.

## Decision

1. **One e2-micro VM in asia-southeast1** on Container-Optimized OS. Two systemd units run
   the containers on a user-defined Docker network. The units and helpers are written by the
   startup script on every boot, because COS keeps `/etc` stateless. No GKE or Cloud Run.
2. **Singapore, not the Always Free US e2-micro.** Supabase, the Qwen endpoint and the
   users are in Southeast Asia. The free tier would save about $9/month and add a Pacific
   round trip to every database query and model call.
3. **The deploy chooses the image; Terraform doesn't.** `deploy.sh` writes the image
   reference to `/var/lib/tatarbot/images/<svc>`, restarts the unit, waits for health and
   rolls back on failure. GitHub Actions reaches it through Workload Identity Federation
   and IAP SSH (OS Login), with no stored keys and no inbound port open to the internet.
4. **Secrets in Secret Manager, read by the host.** iptables keeps the containers away
   from the metadata server, which replaces the IMDSv2 hop limit the AWS design relied on.
5. **GCS through its S3-compatible XML API with an HMAC key**, so the bot's `aws-sdk-s3`
   adapter stays unchanged. Object keys and the Postgres metadata carry over as they are.
6. **linux/amd64 images**: e2 is x86, and GCP's ARM machines (T2A, C4A) are larger and
   cost more than an e2-micro.

## Alternatives considered

* **Always Free e2-micro in us-west1:** about $4/month 24/7, but far from Supabase and
  the model endpoint. It's a one-variable change (`region`, `zone`) if cost wins later.
* **Cloud Run for both services:** the bot needs always-on CPU and a long-lived UDP
  session, so it would pay for an always-allocated instance at several times the VM's
  price. Scaling the runtime to zero would wipe its in-memory history after every idle
  period.
* **GKE Autopilot:** per-pod pricing plus a cluster for two containers; more to operate.
* **COS `gce-container-declaration` (konlet):** deprecated by Google, and it runs only one
  container per VM.
* **Docker Compose on a general-purpose OS:** workable, but the OS would need patching and
  hardening; COS is minimal, read-only and updates itself.
* **Native GCS client in the bot:** removes the long-lived HMAC key, but needs a new
  `ObjectStore` adapter. It can come later without infrastructure changes.

## Consequences

* About $13/month 24/7 (estimate, rates unverified), compared with $15 24/7 or $2.50
  scheduled on AWS.
* Still a single point of failure: a VM restart, a COS update or a deploy takes the bot
  offline for seconds to minutes.
* The bot's music library depends on GCS's S3 compatibility, which is unverified until the
  first live run.
* A long-lived HMAC key exists, in Secret Manager, scoped to one bucket.
* Deploys need IAP SSH with sudo, so the deploy identity can act as root on this one VM.
* CI builds and tests both amd64 and arm64 images until AWS is destroyed.
* The knowledge base's ingestion path (S3 presigned POST, SQS) has no GCP equivalent yet.

## Revisit when

* The bot needs high availability, or a second runtime replica (history moves out of
  memory first).
* The VM runs out of memory: move to e2-small.
* Cost matters more than latency: switch to the free US region.
* The RAG ingestion pipeline is deployed: GCS signed POST and Pub/Sub adapters.
