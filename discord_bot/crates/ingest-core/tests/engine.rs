//! Engine against real Postgres: `#[sqlx::test]` creates a fresh database per test from
//! DATABASE_URL (`make test-db`). Fakes stand in for the pipeline, consumer, artifact store
//! and task queue; the source is served over local HTTP.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::routing::get;
use ingest_core::fetch::HttpsFetcher;
use ingest_core::gcp::LocalArtifacts;
use ingest_core::{
    Artifact, Consumer, Delivery, Engine, EngineSettings, FetchedDecision, Job, JobError,
    JobResult, JobState, Pipeline, PipelineOutput, ProcessError, Producer, Repo, Source,
    SourceKind, SubmitError, SubmitRequest, TaskQueue,
};
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool};
use tokio::sync::Notify;
use uuid::Uuid;

// ---- fakes

#[derive(Default)]
struct RecordingQueue {
    sent: Mutex<Vec<(Uuid, i32)>>,
    fail: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl TaskQueue for RecordingQueue {
    async fn enqueue(&self, job: Uuid, generation: i32) -> Result<(), String> {
        if self.fail.load(Ordering::SeqCst) {
            return Err("queue down".into());
        }
        self.sent.lock().unwrap().push((job, generation));
        Ok(())
    }
}

/// Copies the input to `out.bin`. Scripted failures per call; can block on a gate.
#[derive(Default)]
struct FakePipeline {
    calls: AtomicUsize,
    script: Mutex<Vec<ProcessError>>,
    gate: Mutex<Option<(Arc<Notify>, Arc<Notify>)>>,
}

#[async_trait]
impl Pipeline for FakePipeline {
    fn version(&self) -> &str {
        "fake-v1"
    }
    fn validate_params(&self, params: &Value) -> Result<(), String> {
        if params.get("bad").is_some() {
            Err("bad params".into())
        } else {
            Ok(())
        }
    }
    async fn run(
        &self,
        input: &Path,
        out: &Path,
        _: &Value,
    ) -> Result<PipelineOutput, ProcessError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let gate = self.gate.lock().unwrap().take();
        if let Some((entered, release)) = gate {
            entered.notify_one();
            release.notified().await;
        }
        let next = {
            let mut script = self.script.lock().unwrap();
            if script.is_empty() {
                None
            } else {
                Some(script.remove(0))
            }
        };
        if let Some(err) = next {
            return Err(err);
        }
        let path = out.join("out.bin");
        tokio::fs::copy(input, &path).await.unwrap();
        Ok(PipelineOutput {
            path,
            media_type: "application/octet-stream".into(),
            extension: "bin".into(),
            metadata: json!({ "fake": true }),
        })
    }
}

/// Writes its effects to `test_effects` inside the core's transaction.
#[derive(Default)]
struct FakeConsumer {
    reject_on_finalize: Mutex<Option<ProcessError>>,
    reuse: Mutex<Option<Artifact>>,
}

async fn effect(tx: &mut PgConnection, job: &Job, kind: &str) {
    sqlx::query("INSERT INTO test_effects (job_id, kind) VALUES ($1, $2)")
        .bind(job.id)
        .bind(kind)
        .execute(tx)
        .await
        .unwrap();
}

#[async_trait]
impl Consumer for FakeConsumer {
    async fn on_fetched(&self, _: &Job, _: &str) -> Result<FetchedDecision, ProcessError> {
        Ok(match self.reuse.lock().unwrap().clone() {
            Some(a) => FetchedDecision::Reuse(a),
            None => FetchedDecision::Continue,
        })
    }
    async fn finalize_succeeded(
        &self,
        tx: &mut PgConnection,
        job: &Job,
        _: &JobResult,
    ) -> Result<Value, ProcessError> {
        effect(tx, job, "succeeded").await;
        if let Some(err) = self.reject_on_finalize.lock().unwrap().clone() {
            return Err(err); // the row above must roll back
        }
        Ok(json!({ "recorded": true }))
    }
    async fn finalize_failed(
        &self,
        tx: &mut PgConnection,
        job: &Job,
        error: &JobError,
        _: &Value,
    ) -> Result<(), ProcessError> {
        effect(tx, job, &format!("failed:{}", error.code)).await;
        Ok(())
    }
    async fn finalize_cancelled(
        &self,
        tx: &mut PgConnection,
        job: &Job,
    ) -> Result<(), ProcessError> {
        effect(tx, job, "cancelled").await;
        Ok(())
    }
}

// ---- harness

struct H {
    engine: Arc<Engine>,
    repo: Repo,
    pool: PgPool,
    queue: Arc<RecordingQueue>,
    pipeline: Arc<FakePipeline>,
    consumer: Arc<FakeConsumer>,
    producer: Producer,
    base: String,
    _artifacts: tempfile::TempDir,
}

async fn serve_source() -> String {
    let router = Router::new()
        .route("/song.mp3", get(|| async { "pretend audio bytes" }))
        .route("/huge.mp3", get(|| async { "x".repeat(2048) }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

fn producer() -> Producer {
    Producer {
        id: "tester".into(),
        service_account: "tester@p.iam.gserviceaccount.com".into(),
        tenant_prefix: "t:".into(),
        allowed_pipelines: vec!["fake.v1".into()],
        allowed_source_kinds: vec!["https_url".into()],
        source_url_hosts: vec!["127.0.0.1".into()],
        max_source_bytes: 1024,
        max_active_per_actor: 3,
        max_jobs_per_tenant_hour: 5,
        enabled: true,
    }
}

async fn harness(pool: PgPool, settings: EngineSettings) -> H {
    ingest_core::migrate::migrate(&pool).await.unwrap();
    sqlx::query("CREATE TABLE test_effects (job_id UUID NOT NULL, kind TEXT NOT NULL)")
        .execute(&pool)
        .await
        .unwrap();
    let repo = Repo::new(pool.clone());
    let producer = producer();
    repo.upsert_producer(&producer).await.unwrap();
    let queue = Arc::new(RecordingQueue::default());
    let pipeline = Arc::new(FakePipeline::default());
    let consumer = Arc::new(FakeConsumer::default());
    let artifacts = tempfile::tempdir().unwrap();
    let fetcher = HttpsFetcher::new(Duration::from_secs(5))
        .unwrap()
        .allowing_loopback_http();
    let engine = Engine::builder(
        repo.clone(),
        Arc::new(LocalArtifacts(artifacts.path().to_owned())),
        queue.clone(),
        fetcher,
    )
    .pipeline("fake.v1", pipeline.clone())
    .consumer("tester", consumer.clone())
    .settings(settings)
    .build();
    H {
        engine: Arc::new(engine),
        repo,
        pool,
        queue,
        pipeline,
        consumer,
        producer,
        base: serve_source().await,
        _artifacts: artifacts,
    }
}

impl H {
    fn request(&self, key: &str) -> SubmitRequest {
        SubmitRequest {
            idempotency_key: key.into(),
            tenant: "t:guild:1".into(),
            actor: format!("t:user:{key}"),
            pipeline: "fake.v1".into(),
            params: json!({}),
            source: Source {
                kind: SourceKind::HttpsUrl,
                uri: format!("{}/song.mp3", self.base),
                declared_size: Some(19),
            },
        }
    }

    async fn submit(&self, key: &str) -> Job {
        self.engine
            .submit(&self.producer, &self.request(key))
            .await
            .unwrap()
            .0
    }

    async fn job(&self, id: Uuid) -> Job {
        self.repo.job(id).await.unwrap().unwrap()
    }

    async fn effects(&self, id: Uuid) -> Vec<String> {
        sqlx::query_scalar("SELECT kind FROM test_effects WHERE job_id = $1 ORDER BY kind")
            .bind(id)
            .fetch_all(&self.pool)
            .await
            .unwrap()
    }

    async fn events(&self, id: Uuid) -> Vec<String> {
        sqlx::query_scalar("SELECT type FROM ingest.job_events WHERE job_id = $1 ORDER BY id")
            .bind(id)
            .fetch_all(&self.pool)
            .await
            .unwrap()
    }

    async fn expire_lease(&self, id: Uuid) {
        sqlx::query(
            "UPDATE ingest.jobs SET lease_expires_at = now() - interval '1 second' WHERE id = $1",
        )
        .bind(id)
        .execute(&self.pool)
        .await
        .unwrap();
    }
}

fn fast() -> EngineSettings {
    EngineSettings::default()
}

// ---- submission

#[sqlx::test(migrations = false)]
#[ignore = "needs DATABASE_URL (make test-db)"]
async fn pg_submit_is_idempotent_per_producer_key(pool: PgPool) {
    let h = harness(pool, fast()).await;
    let req = h.request("k1");
    let (a, created_a) = h.engine.submit(&h.producer, &req).await.unwrap();
    let (b, created_b) = h.engine.submit(&h.producer, &req).await.unwrap();
    let (c, _) = h.engine.submit(&h.producer, &req).await.unwrap();
    assert!(created_a && !created_b);
    assert_eq!((a.id, b.id), (c.id, c.id));
    assert_eq!(h.queue.sent.lock().unwrap().as_slice(), &[(a.id, 0)]);
    assert!(h.job(a.id).await.enqueued_at.is_some());
}

#[sqlx::test(migrations = false)]
#[ignore = "needs DATABASE_URL (make test-db)"]
async fn pg_submit_enforces_policy_and_admission_limits(pool: PgPool) {
    let h = harness(pool, fast()).await;
    let policy = |mut f: Box<dyn FnMut(&mut SubmitRequest)>| {
        let mut r = h.request("p");
        f(&mut r);
        r
    };
    let cases: Vec<SubmitRequest> = vec![
        policy(Box::new(|r| r.tenant = "other:1".into())),
        policy(Box::new(|r| r.pipeline = "nope".into())),
        policy(Box::new(|r| r.params = json!({ "bad": 1 }))),
        policy(Box::new(|r| r.source.declared_size = Some(4096))),
        policy(Box::new(|r| {
            r.source.uri = "https://evil.test/a.mp3".into()
        })),
        policy(Box::new(|r| r.source.kind = SourceKind::GcsObject)),
    ];
    for req in cases {
        assert!(
            matches!(
                h.engine.submit(&h.producer, &req).await,
                Err(SubmitError::Policy(_))
            ),
            "{req:?}"
        );
    }

    // max_active_per_actor = 3 (same actor), then the tenant's 5 per hour.
    let mut same_actor = h.request("a0");
    for i in 0..3 {
        same_actor.idempotency_key = format!("a{i}");
        h.engine.submit(&h.producer, &same_actor).await.unwrap();
    }
    same_actor.idempotency_key = "a3".into();
    assert_eq!(
        h.engine.submit(&h.producer, &same_actor).await.unwrap_err(),
        SubmitError::TooManyActive(3)
    );
    h.submit("b1").await;
    h.submit("b2").await;
    assert_eq!(
        h.engine
            .submit(&h.producer, &h.request("b3"))
            .await
            .unwrap_err(),
        SubmitError::TenantRateLimited(5)
    );

    let mut disabled = h.producer.clone();
    disabled.enabled = false;
    assert_eq!(
        h.engine
            .submit(&disabled, &h.request("x"))
            .await
            .unwrap_err(),
        SubmitError::ProducerDisabled
    );
}

#[sqlx::test(migrations = false)]
#[ignore = "needs DATABASE_URL (make test-db)"]
async fn pg_a_failed_enqueue_is_recovered_by_the_reaper(pool: PgPool) {
    let h = harness(pool, fast()).await;
    h.queue.fail.store(true, Ordering::SeqCst);
    let job = h.submit("k").await;
    assert!(h.job(job.id).await.enqueued_at.is_none());
    h.queue.fail.store(false, Ordering::SeqCst);
    let report = h.engine.reap().await.unwrap();
    assert_eq!(report.enqueued, 1);
    assert_eq!(h.queue.sent.lock().unwrap().as_slice(), &[(job.id, 1)]);
    assert_eq!(h.engine.reap().await.unwrap().enqueued, 0); // the task exists now
}

// ---- processing and transactional consistency

#[sqlx::test(migrations = false)]
#[ignore = "needs DATABASE_URL (make test-db)"]
async fn pg_success_commits_result_event_and_consumer_effects_together(pool: PgPool) {
    let h = harness(pool, fast()).await;
    let job = h.submit("k").await;
    assert_eq!(h.engine.process(job.id).await.unwrap(), Delivery::Done);
    let done = h.job(job.id).await;
    assert_eq!(done.state, JobState::Succeeded);
    assert_eq!(done.attempts, 1);
    assert!(done.lease_owner.is_none());
    let result: JobResult = serde_json::from_value(done.result.unwrap()).unwrap();
    assert_eq!(result.pipeline_version, "fake-v1");
    assert_eq!(result.artifacts[0].size, 19);
    assert_eq!(result.consumer, json!({ "recorded": true }));
    assert_eq!(
        done.source_sha256.as_deref(),
        Some(result.source_sha256.as_str())
    );
    assert_eq!(h.effects(job.id).await, ["succeeded"]);
    assert_eq!(h.events(job.id).await, ["succeeded"]);

    // Redelivery after success: nothing happens again.
    assert_eq!(h.engine.process(job.id).await.unwrap(), Delivery::Done);
    assert_eq!(h.pipeline.calls.load(Ordering::SeqCst), 1);
    assert_eq!(h.effects(job.id).await, ["succeeded"]);
}

#[sqlx::test(migrations = false)]
#[ignore = "needs DATABASE_URL (make test-db)"]
async fn pg_consumer_rejection_rolls_back_its_writes_and_fails_the_job(pool: PgPool) {
    let h = harness(pool, fast()).await;
    *h.consumer.reject_on_finalize.lock().unwrap() =
        Some(ProcessError::permanent("quota_exceeded", None));
    let job = h.submit("k").await;
    assert_eq!(h.engine.process(job.id).await.unwrap(), Delivery::Done);
    let done = h.job(job.id).await;
    assert_eq!(done.state, JobState::Failed);
    assert_eq!(done.error_code.as_deref(), Some("quota_exceeded"));
    // The "succeeded" row written before the rejection was rolled back.
    assert_eq!(h.effects(job.id).await, ["failed:quota_exceeded"]);
    assert_eq!(h.events(job.id).await, ["failed"]);
}

#[sqlx::test(migrations = false)]
#[ignore = "needs DATABASE_URL (make test-db)"]
async fn pg_reuse_skips_the_pipeline(pool: PgPool) {
    let h = harness(pool, fast()).await;
    *h.consumer.reuse.lock().unwrap() = Some(Artifact {
        uri: "gs://b/k".into(),
        key: "k".into(),
        sha256: None,
        size: 7,
        media_type: "audio/ogg".into(),
        metadata: json!({}),
    });
    let job = h.submit("k").await;
    h.engine.process(job.id).await.unwrap();
    let result: JobResult = serde_json::from_value(h.job(job.id).await.result.unwrap()).unwrap();
    assert!(result.reused);
    assert_eq!(h.pipeline.calls.load(Ordering::SeqCst), 0);
}

#[sqlx::test(migrations = false)]
#[ignore = "needs DATABASE_URL (make test-db)"]
async fn pg_a_stale_attempt_cannot_commit_after_takeover(pool: PgPool) {
    let h = harness(pool, fast()).await;
    let job = h.submit("k").await;
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    *h.pipeline.gate.lock().unwrap() = Some((entered.clone(), release.clone()));

    // Attempt 1 claims and blocks inside the pipeline.
    let engine = h.engine.clone();
    let slow = tokio::spawn(async move { engine.process(job.id).await });
    entered.notified().await;
    // Its lease expires (instance frozen); attempt 2 takes over and succeeds.
    h.expire_lease(job.id).await;
    assert_eq!(h.engine.process(job.id).await.unwrap(), Delivery::Done);
    assert_eq!(h.job(job.id).await.state, JobState::Succeeded);
    // Attempt 1 wakes up and tries to commit: the fence refuses, nothing is applied twice.
    release.notify_one();
    assert_eq!(slow.await.unwrap().unwrap(), Delivery::Done);
    let done = h.job(job.id).await;
    assert_eq!((done.state, done.attempts), (JobState::Succeeded, 2));
    assert_eq!(h.effects(job.id).await, ["succeeded"]);
    assert_eq!(h.events(job.id).await, ["succeeded"]);
}

#[sqlx::test(migrations = false)]
#[ignore = "needs DATABASE_URL (make test-db)"]
async fn pg_concurrent_duplicate_deliveries_process_once(pool: PgPool) {
    let h = harness(pool, fast()).await;
    let job = h.submit("k").await;
    let runs = (0..5).map(|_| {
        let engine = h.engine.clone();
        tokio::spawn(async move { engine.process(job.id).await.unwrap() })
    });
    for r in futures::future::join_all(runs).await {
        assert_eq!(r.unwrap(), Delivery::Done);
    }
    assert_eq!(h.pipeline.calls.load(Ordering::SeqCst), 1);
    assert_eq!(h.effects(job.id).await, ["succeeded"]);
}

// ---- retries

#[sqlx::test(migrations = false)]
#[ignore = "needs DATABASE_URL (make test-db)"]
async fn pg_transient_failures_retry_then_exhaust(pool: PgPool) {
    let h = harness(pool, fast()).await;
    *h.pipeline.script.lock().unwrap() = (0..4)
        .map(|i| ProcessError::Transient(format!("blip {i}")))
        .collect();
    let job = h.submit("k").await;
    for attempt in 1..=3 {
        assert_eq!(
            h.engine.process(job.id).await.unwrap(),
            Delivery::Retry,
            "attempt {attempt}"
        );
        let j = h.job(job.id).await;
        assert_eq!((j.state, j.attempts), (JobState::Queued, attempt));
    }
    // 4th = max_attempts: recorded as failed and acknowledged.
    assert_eq!(h.engine.process(job.id).await.unwrap(), Delivery::Done);
    let done = h.job(job.id).await;
    assert_eq!(done.state, JobState::Failed);
    assert_eq!(done.error_code.as_deref(), Some("retries_exhausted"));
    assert_eq!(h.effects(job.id).await, ["failed:retries_exhausted"]);
}

#[sqlx::test(migrations = false)]
#[ignore = "needs DATABASE_URL (make test-db)"]
async fn pg_transient_failure_then_success(pool: PgPool) {
    let h = harness(pool, fast()).await;
    *h.pipeline.script.lock().unwrap() = vec![ProcessError::Transient("gcs 503".into())];
    let job = h.submit("k").await;
    assert_eq!(h.engine.process(job.id).await.unwrap(), Delivery::Retry);
    assert_eq!(h.engine.process(job.id).await.unwrap(), Delivery::Done);
    assert_eq!(h.job(job.id).await.state, JobState::Succeeded);
    assert_eq!(h.effects(job.id).await, ["succeeded"]);
}

#[sqlx::test(migrations = false)]
#[ignore = "needs DATABASE_URL (make test-db)"]
async fn pg_resource_limits_retry_once_then_fail(pool: PgPool) {
    let h = harness(pool, fast()).await;
    *h.pipeline.script.lock().unwrap() = vec![
        ProcessError::Resource("pthread_create EAGAIN".into()),
        ProcessError::Resource("SIGXCPU".into()),
    ];
    let job = h.submit("k").await;
    assert_eq!(h.engine.process(job.id).await.unwrap(), Delivery::Retry);
    assert_eq!(h.engine.process(job.id).await.unwrap(), Delivery::Done);
    let done = h.job(job.id).await;
    assert_eq!(done.error_code.as_deref(), Some("processing_limit"));
}

#[sqlx::test(migrations = false)]
#[ignore = "needs DATABASE_URL (make test-db)"]
async fn pg_permanent_failures_are_not_retried(pool: PgPool) {
    let h = harness(pool, fast()).await;
    let mut req = h.request("k");
    req.source.uri = format!("{}/huge.mp3", h.base);
    req.source.declared_size = None;
    let (job, _) = h.engine.submit(&h.producer, &req).await.unwrap();
    assert_eq!(h.engine.process(job.id).await.unwrap(), Delivery::Done);
    let done = h.job(job.id).await;
    assert_eq!((done.state, done.attempts), (JobState::Failed, 1));
    assert_eq!(done.error_code.as_deref(), Some("too_large"));
}

// ---- cancellation

#[sqlx::test(migrations = false)]
#[ignore = "needs DATABASE_URL (make test-db)"]
async fn pg_cancelling_a_queued_job_never_runs_it(pool: PgPool) {
    let h = harness(pool, fast()).await;
    let job = h.submit("k").await;
    let after = h.engine.cancel("tester", job.id).await.unwrap().unwrap();
    assert_eq!(after.state, JobState::Cancelled);
    assert_eq!(h.engine.process(job.id).await.unwrap(), Delivery::Done);
    assert_eq!(h.pipeline.calls.load(Ordering::SeqCst), 0);
    assert_eq!(h.effects(job.id).await, ["cancelled"]);
    // Another producer can't see or cancel it.
    assert!(
        h.engine
            .cancel("someone-else", job.id)
            .await
            .unwrap()
            .is_none()
    );
}

#[sqlx::test(migrations = false)]
#[ignore = "needs DATABASE_URL (make test-db)"]
async fn pg_cancelling_a_processing_job_stops_before_commit(pool: PgPool) {
    let h = harness(pool, fast()).await;
    let job = h.submit("k").await;
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    *h.pipeline.gate.lock().unwrap() = Some((entered.clone(), release.clone()));
    let engine = h.engine.clone();
    let running = tokio::spawn(async move { engine.process(job.id).await });
    entered.notified().await;
    let flagged = h.engine.cancel("tester", job.id).await.unwrap().unwrap();
    assert_eq!(flagged.state, JobState::Processing);
    assert!(flagged.cancel_requested);
    release.notify_one();
    assert_eq!(running.await.unwrap().unwrap(), Delivery::Done);
    assert_eq!(h.job(job.id).await.state, JobState::Cancelled);
    assert_eq!(h.effects(job.id).await, ["cancelled"]);
}

// ---- kill switch and reaper

#[sqlx::test(migrations = false)]
#[ignore = "needs DATABASE_URL (make test-db)"]
async fn pg_kill_switch_pauses_without_spending_attempts(pool: PgPool) {
    let h = harness(pool, fast()).await;
    let job = h.submit("k").await;
    h.repo
        .set_switch("producer:tester", true, "maintenance", "operator:test")
        .await
        .unwrap();
    assert!(matches!(
        h.engine.submit(&h.producer, &h.request("k2")).await,
        Err(SubmitError::Paused(_))
    ));
    assert_eq!(h.engine.process(job.id).await.unwrap(), Delivery::Done);
    let paused = h.job(job.id).await;
    assert_eq!((paused.state, paused.attempts), (JobState::Queued, 0));
    assert!(paused.enqueued_at.is_none());
    assert_eq!(h.engine.reap().await.unwrap().enqueued, 0); // not while paused

    h.repo
        .set_switch("producer:tester", false, "done", "operator:test")
        .await
        .unwrap();
    assert_eq!(h.engine.reap().await.unwrap().enqueued, 1);
    assert_eq!(h.engine.process(job.id).await.unwrap(), Delivery::Done);
    assert_eq!(h.job(job.id).await.state, JobState::Succeeded);
}

#[sqlx::test(migrations = false)]
#[ignore = "needs DATABASE_URL (make test-db)"]
async fn pg_budget_brake_pauses_everything(pool: PgPool) {
    let h = harness(pool, fast()).await;
    assert!(!h.engine.budget_update(5.0, 10.0).await.unwrap());
    assert!(h.engine.budget_update(10.0, 10.0).await.unwrap());
    let (scope, reason) = h.repo.paused("tester", "fake.v1").await.unwrap().unwrap();
    assert_eq!(scope, "global");
    assert!(reason.contains("budget"));
}

#[sqlx::test(migrations = false)]
#[ignore = "needs DATABASE_URL (make test-db)"]
async fn pg_reaper_recovers_dead_attempts_and_fails_what_cannot_finish(pool: PgPool) {
    let h = harness(pool, fast()).await;
    // A: attempt died mid-way (lease expired) → requeued and given a new task.
    let a = h.submit("a").await;
    h.repo
        .claim(a.id, Uuid::new_v4(), 600)
        .await
        .unwrap()
        .unwrap();
    h.expire_lease(a.id).await;
    // B: died on its last attempt → failed.
    let b = h.submit("b").await;
    sqlx::query("UPDATE ingest.jobs SET attempts = max_attempts - 1 WHERE id = $1")
        .bind(b.id)
        .execute(&h.pool)
        .await
        .unwrap();
    h.repo
        .claim(b.id, Uuid::new_v4(), 600)
        .await
        .unwrap()
        .unwrap();
    h.expire_lease(b.id).await;
    // C: queued and untouched for over an hour → timed out.
    let c = h.submit("c").await;
    sqlx::query("UPDATE ingest.jobs SET updated_at = now() - interval '2 hours' WHERE id = $1")
        .bind(c.id)
        .execute(&h.pool)
        .await
        .unwrap();

    let report = h.engine.reap().await.unwrap();
    assert_eq!(
        (
            report.requeued_expired,
            report.failed_exhausted,
            report.timed_out
        ),
        (1, 1, 1)
    );
    assert!(report.enqueued >= 1);
    assert_eq!(h.job(a.id).await.state, JobState::Queued);
    assert!(
        h.queue
            .sent
            .lock()
            .unwrap()
            .iter()
            .any(|(id, g)| *id == a.id && *g == 1)
    );
    assert_eq!(h.effects(b.id).await, ["failed:retries_exhausted"]);
    assert_eq!(h.effects(c.id).await, ["failed:timed_out"]);
    // A finishes normally afterwards.
    assert_eq!(h.engine.process(a.id).await.unwrap(), Delivery::Done);
    assert_eq!(h.job(a.id).await.state, JobState::Succeeded);
}

#[sqlx::test(migrations = false)]
#[ignore = "needs DATABASE_URL (make test-db)"]
async fn pg_migrations_are_idempotent(pool: PgPool) {
    assert_eq!(ingest_core::migrate::migrate(&pool).await.unwrap().len(), 1);
    assert!(
        ingest_core::migrate::migrate(&pool)
            .await
            .unwrap()
            .is_empty()
    );
}
