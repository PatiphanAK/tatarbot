//! The job lifecycle (ADR 0004 §2, §6): admission, one attempt per delivery under a fenced
//! lease, consumer hooks inside the state-changing transactions, retries, cancellation, the
//! reaper and the kill switch.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::Connection;
use uuid::Uuid;

use crate::fetch::HttpsFetcher;
use crate::model::{
    Artifact, Job, JobError, JobResult, JobState, ProcessError, Producer, SourceKind,
    SubmitRequest, codes, hex,
};
use crate::ports::{ArtifactStore, Consumer, FetchedDecision, Pipeline, TaskQueue};
use crate::repo::{Admission, ReapReport, Repo, RepoError};

#[derive(Debug, Clone)]
pub struct EngineSettings {
    /// Longer than the Cloud Run request timeout, so a live attempt never loses its lease.
    pub lease_secs: i64,
    pub max_attempts: i32,
    /// Resource-limit failures are retried this many times in total, then permanent.
    pub max_resource_attempts: i32,
    /// A queued job untouched this long is failed `timed_out`.
    pub queued_timeout_secs: i64,
    /// A task older than this is assumed dead (Cloud Tasks gave up), so a new one is made.
    pub task_dead_after_secs: i64,
    pub retention_days: i32,
}

impl Default for EngineSettings {
    fn default() -> Self {
        Self {
            lease_secs: 12 * 60,
            max_attempts: 4,
            max_resource_attempts: 2,
            queued_timeout_secs: 60 * 60,
            task_dead_after_secs: 70 * 60,
            retention_days: 30,
        }
    }
}

/// Why `submit` refused.
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum SubmitError {
    #[error("producer disabled")]
    ProducerDisabled,
    #[error("policy: {0}")]
    Policy(String),
    #[error("too many unfinished jobs for this actor (limit {0})")]
    TooManyActive(u32),
    #[error("too many jobs for this tenant in the last hour (limit {0})")]
    TenantRateLimited(u32),
    #[error("paused: {0}")]
    Paused(String),
    #[error("internal: {0}")]
    Internal(String),
}

impl From<RepoError> for SubmitError {
    fn from(e: RepoError) -> Self {
        Self::Internal(e.to_string())
    }
}

/// What `/internal/process` answers: 2xx = done with this delivery, 503 = retry it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    Done,
    Retry,
}

pub struct Engine {
    repo: Repo,
    pipelines: HashMap<String, Arc<dyn Pipeline>>,
    consumers: HashMap<String, Arc<dyn Consumer>>,
    artifacts: Arc<dyn ArtifactStore>,
    queue: Arc<dyn TaskQueue>,
    fetcher: HttpsFetcher,
    settings: EngineSettings,
    /// Where the artifact store keeps objects, for `Artifact::uri` defaults.
    scratch_root: Option<std::path::PathBuf>,
}

pub struct EngineBuilder {
    engine: Engine,
}

impl EngineBuilder {
    pub fn pipeline(mut self, id: &str, pipeline: Arc<dyn Pipeline>) -> Self {
        self.engine.pipelines.insert(id.to_owned(), pipeline);
        self
    }

    pub fn consumer(mut self, producer: &str, consumer: Arc<dyn Consumer>) -> Self {
        self.engine.consumers.insert(producer.to_owned(), consumer);
        self
    }

    pub fn settings(mut self, settings: EngineSettings) -> Self {
        self.engine.settings = settings;
        self
    }

    /// Parent directory for per-attempt scratch space (default: the system temp dir).
    pub fn scratch_root(mut self, dir: std::path::PathBuf) -> Self {
        self.engine.scratch_root = Some(dir);
        self
    }

    pub fn build(self) -> Engine {
        self.engine
    }
}

/// The outcome of one attempt, before it is committed.
enum Attempt {
    Succeeded(JobResult),
    Cancelled,
}

impl Engine {
    pub fn builder(
        repo: Repo,
        artifacts: Arc<dyn ArtifactStore>,
        queue: Arc<dyn TaskQueue>,
        fetcher: HttpsFetcher,
    ) -> EngineBuilder {
        EngineBuilder {
            engine: Engine {
                repo,
                pipelines: HashMap::new(),
                consumers: HashMap::new(),
                artifacts,
                queue,
                fetcher,
                settings: EngineSettings::default(),
                scratch_root: None,
            },
        }
    }

    pub fn repo(&self) -> &Repo {
        &self.repo
    }

    // ---- submission

    /// Validates against the producer's policy, admits idempotently, then tries to enqueue.
    /// A failed enqueue still returns the job: the reaper creates the task later.
    pub async fn submit(
        &self,
        producer: &Producer,
        req: &SubmitRequest,
    ) -> Result<(Job, bool), SubmitError> {
        if !producer.enabled {
            return Err(SubmitError::ProducerDisabled);
        }
        self.check_policy(producer, req)?;
        if let Some((scope, reason)) = self.repo.paused(&producer.id, &req.pipeline).await? {
            return Err(SubmitError::Paused(format!("{scope}: {reason}")));
        }
        let job = match self
            .repo
            .submit(producer, req, self.settings.max_attempts)
            .await?
        {
            Admission::Existing(job) => return Ok((job, false)),
            Admission::Created(job) => job,
            Admission::TooManyActive { limit } => return Err(SubmitError::TooManyActive(limit)),
            Admission::TenantRateLimited { limit } => {
                return Err(SubmitError::TenantRateLimited(limit));
            }
        };
        match self.queue.enqueue(job.id, job.enqueue_generation).await {
            Ok(()) => {
                self.repo
                    .mark_enqueued(job.id, job.enqueue_generation)
                    .await?
            }
            Err(err) => {
                tracing::warn!(job = %job.id, %err, "enqueue failed; the reaper will retry")
            }
        }
        Ok((job, true))
    }

    fn check_policy(&self, producer: &Producer, req: &SubmitRequest) -> Result<(), SubmitError> {
        let deny = |m: &str| Err(SubmitError::Policy(m.to_owned()));
        if !req.tenant.starts_with(&producer.tenant_prefix) {
            return deny("tenant outside the producer's prefix");
        }
        if req.actor.is_empty() || req.actor.len() > 200 || req.tenant.len() > 200 {
            return deny("bad tenant or actor");
        }
        if req.idempotency_key.is_empty() || req.idempotency_key.len() > 200 {
            return deny("bad idempotency key");
        }
        if !producer
            .allowed_pipelines
            .iter()
            .any(|p| p == &req.pipeline)
        {
            return deny("pipeline not allowed");
        }
        let Some(pipeline) = self.pipelines.get(&req.pipeline) else {
            return deny("unknown pipeline");
        };
        pipeline
            .validate_params(&req.params)
            .map_err(SubmitError::Policy)?;
        if !producer
            .allowed_source_kinds
            .iter()
            .any(|k| k == req.source.kind.as_str())
        {
            return deny("source kind not allowed");
        }
        if req
            .source
            .declared_size
            .is_some_and(|s| s > producer.max_source_bytes)
        {
            return deny("declared size above the producer's limit");
        }
        match req.source.kind {
            SourceKind::HttpsUrl => {
                self.fetcher
                    .check_url(producer, &req.source.uri)
                    .map_err(|_| SubmitError::Policy("source URL not allowed".into()))?;
            }
            SourceKind::GcsObject => return deny("gcs_object sources are not supported yet"),
        }
        Ok(())
    }

    pub async fn cancel(&self, producer: &str, id: Uuid) -> Result<Option<Job>, RepoError> {
        let Some(job) = self.repo.request_cancel(id, producer).await? else {
            return self
                .repo
                .job(id)
                .await
                .map(|j| j.filter(|j| j.producer_id == producer));
        };
        if job.state == JobState::Queued {
            self.cancel_queued(id).await?;
        }
        self.repo.job(id).await
    }

    /// queued → cancelled with the consumer's hook, if still queued.
    async fn cancel_queued(&self, id: Uuid) -> Result<(), RepoError> {
        let mut tx = self.repo.pool().begin().await?;
        let Some(job) = Repo::lock_in_state(&mut tx, id, JobState::Queued).await? else {
            return Ok(());
        };
        if let Some(consumer) = self.consumers.get(&job.producer_id)
            && let Err(err) = consumer.finalize_cancelled(&mut tx, &job).await
        {
            tracing::warn!(job = %id, %err, "consumer cancel hook failed; will retry");
            return Ok(());
        }
        Repo::finish(&mut tx, id, JobState::Cancelled, None, None).await?;
        tx.commit().await?;
        Ok(())
    }

    // ---- processing

    /// Handles one delivery of "process `id`".
    pub async fn process(&self, id: Uuid) -> Result<Delivery, RepoError> {
        let Some(job) = self.repo.job(id).await? else {
            tracing::warn!(job = %id, "delivery for an unknown job; dropped");
            return Ok(Delivery::Done);
        };
        if job.state.is_terminal() {
            return Ok(Delivery::Done);
        }
        if let Some((scope, _)) = self.repo.paused(&job.producer_id, &job.pipeline).await? {
            // Don't spend attempts while paused; the reaper re-enqueues on resume.
            tracing::info!(job = %id, scope, "paused; leaving the job queued");
            self.repo.mark_needs_enqueue(id).await?;
            return Ok(Delivery::Done);
        }
        if job.state == JobState::Queued && job.cancel_requested {
            self.cancel_queued(id).await?;
            return Ok(Delivery::Done);
        }
        let owner = Uuid::new_v4();
        let Some(job) = self.repo.claim(id, owner, self.settings.lease_secs).await? else {
            // Someone holds a live lease, or attempts are used up (the reaper fails those).
            return Ok(Delivery::Done);
        };
        tracing::info!(job = %id, attempt = job.attempts, pipeline = %job.pipeline, "processing");

        let attempt = self.attempt(&job, owner).await;
        let committed = match attempt {
            Ok(Attempt::Succeeded(result)) => self.commit_success(&job, owner, result).await,
            Ok(Attempt::Cancelled) => self.commit_cancelled(&job, owner).await,
            Err(err) => Err(err),
        };
        match committed {
            Ok(()) => Ok(Delivery::Done),
            Err(ProcessError::Permanent { code, detail, data }) => {
                self.commit_failure(&job, owner, JobError { code, detail }, data)
                    .await
            }
            Err(ProcessError::Resource(reason))
                if job.attempts >= self.settings.max_resource_attempts =>
            {
                tracing::warn!(job = %id, %reason, "resource limit again; failing");
                let error = JobError {
                    code: codes::PROCESSING_LIMIT.into(),
                    detail: None,
                };
                self.commit_failure(&job, owner, error, Value::Null).await
            }
            Err(ProcessError::Transient(reason) | ProcessError::Resource(reason))
                if job.attempts >= job.max_attempts =>
            {
                tracing::warn!(job = %id, %reason, "last attempt failed; giving up");
                let error = JobError {
                    code: codes::RETRIES_EXHAUSTED.into(),
                    detail: None,
                };
                self.commit_failure(&job, owner, error, Value::Null).await
            }
            Err(ProcessError::Transient(reason) | ProcessError::Resource(reason)) => {
                tracing::warn!(job = %id, attempt = job.attempts, %reason, "attempt failed; retrying");
                self.repo.release(id, owner, &reason).await?;
                Ok(Delivery::Retry)
            }
        }
    }

    async fn attempt(&self, job: &Job, owner: Uuid) -> Result<Attempt, ProcessError> {
        let transient = |e: RepoError| ProcessError::Transient(e.to_string());
        let producer = self
            .repo
            .producer(&job.producer_id)
            .await
            .map_err(transient)?
            .ok_or_else(|| ProcessError::permanent(codes::POLICY_DENIED, None))?;
        let pipeline = self
            .pipelines
            .get(&job.pipeline)
            .ok_or_else(|| ProcessError::permanent(codes::POLICY_DENIED, None))?;
        let consumer = self.consumers.get(&job.producer_id);
        if job.source.kind != SourceKind::HttpsUrl {
            return Err(ProcessError::permanent(codes::UNSUPPORTED_SOURCE, None));
        }

        let scratch = match &self.scratch_root {
            Some(root) => tempfile::tempdir_in(root),
            None => tempfile::tempdir(),
        }
        .map_err(|e| ProcessError::Transient(format!("scratch dir: {e}")))?;
        let input = scratch.path().join("input");
        let fetched = self
            .fetcher
            .fetch(&producer, &job.source.uri, &input)
            .await?;
        self.repo
            .set_source_sha256(job.id, owner, &fetched.sha256)
            .await
            .map_err(transient)?;
        if self
            .repo
            .cancel_requested(job.id)
            .await
            .map_err(transient)?
        {
            return Ok(Attempt::Cancelled);
        }

        let decision = match consumer {
            Some(c) => c.on_fetched(job, &fetched.sha256).await?,
            None => FetchedDecision::Continue,
        };
        let (artifact, reused) = match decision {
            FetchedDecision::Reject(err) => return Err(err),
            FetchedDecision::Reuse(artifact) => (artifact, true),
            FetchedDecision::Continue => {
                let out_dir = scratch.path().join("out");
                tokio::fs::create_dir_all(&out_dir)
                    .await
                    .map_err(|e| ProcessError::Transient(e.to_string()))?;
                let output = pipeline.run(&input, &out_dir, &job.params).await?;
                if self
                    .repo
                    .cancel_requested(job.id)
                    .await
                    .map_err(transient)?
                {
                    return Ok(Attempt::Cancelled);
                }
                let key = consumer
                    .and_then(|c| c.output_key(job, &fetched.sha256, &output.extension))
                    .unwrap_or_else(|| {
                        format!(
                            "{}/{}.{}",
                            job.producer_id, fetched.sha256, output.extension
                        )
                    });
                let (size, sha256) = file_digest(&output.path).await?;
                let uri = self
                    .artifacts
                    .put_if_absent(&key, &output.path, &output.media_type)
                    .await?;
                (
                    Artifact {
                        uri,
                        key,
                        sha256: Some(sha256),
                        size,
                        media_type: output.media_type,
                        metadata: output.metadata,
                    },
                    false,
                )
            }
        };
        Ok(Attempt::Succeeded(JobResult {
            pipeline_version: pipeline.version().to_owned(),
            source_sha256: fetched.sha256,
            reused,
            artifacts: vec![artifact],
            consumer: Value::Null,
        }))
    }

    async fn commit_success(
        &self,
        job: &Job,
        owner: Uuid,
        mut result: JobResult,
    ) -> Result<(), ProcessError> {
        let db = |e: sqlx::Error| ProcessError::Transient(e.to_string());
        let repo = |e: RepoError| ProcessError::Transient(e.to_string());
        let mut tx = self.repo.pool().begin().await.map_err(db)?;
        let Some(locked) = Repo::fence(&mut tx, job.id, owner).await.map_err(repo)? else {
            tracing::warn!(job = %job.id, "lease lost before commit; discarding this attempt");
            return Ok(());
        };
        if locked.cancel_requested {
            drop(tx);
            return self.commit_cancelled(job, owner).await;
        }
        if let Some(consumer) = self.consumers.get(&job.producer_id) {
            // An Err rolls back everything the consumer wrote (tx is dropped).
            result.consumer = consumer
                .finalize_succeeded(&mut tx, &locked, &result)
                .await?;
        }
        let value =
            serde_json::to_value(&result).map_err(|e| ProcessError::Transient(e.to_string()))?;
        Repo::finish(&mut tx, job.id, JobState::Succeeded, Some(&value), None)
            .await
            .map_err(repo)?;
        tx.commit().await.map_err(db)?;
        tracing::info!(job = %job.id, reused = result.reused, "succeeded");
        Ok(())
    }

    async fn commit_cancelled(&self, job: &Job, owner: Uuid) -> Result<(), ProcessError> {
        let db = |e: sqlx::Error| ProcessError::Transient(e.to_string());
        let repo = |e: RepoError| ProcessError::Transient(e.to_string());
        let mut tx = self.repo.pool().begin().await.map_err(db)?;
        let Some(locked) = Repo::fence(&mut tx, job.id, owner).await.map_err(repo)? else {
            return Ok(());
        };
        if let Some(consumer) = self.consumers.get(&job.producer_id) {
            consumer.finalize_cancelled(&mut tx, &locked).await?;
        }
        Repo::finish(&mut tx, job.id, JobState::Cancelled, None, None)
            .await
            .map_err(repo)?;
        tx.commit().await.map_err(db)?;
        tracing::info!(job = %job.id, "cancelled");
        Ok(())
    }

    /// Records a terminal failure under the fence; if that can't commit, releases the job so
    /// the delivery (or the reaper) tries again.
    async fn commit_failure(
        &self,
        job: &Job,
        owner: Uuid,
        error: JobError,
        data: Value,
    ) -> Result<Delivery, RepoError> {
        let mut tx = self.repo.pool().begin().await?;
        let Some(locked) = Repo::fence(&mut tx, job.id, owner).await? else {
            return Ok(Delivery::Done);
        };
        if let Some(consumer) = self.consumers.get(&job.producer_id)
            && let Err(err) = consumer
                .finalize_failed(&mut tx, &locked, &error, &data)
                .await
        {
            drop(tx);
            tracing::warn!(job = %job.id, %err, "consumer failure hook failed; retrying");
            self.repo.release(job.id, owner, &err.to_string()).await?;
            return Ok(Delivery::Retry);
        }
        Repo::finish(&mut tx, job.id, JobState::Failed, None, Some(&error)).await?;
        tx.commit().await?;
        tracing::info!(job = %job.id, code = %error.code, "failed");
        Ok(Delivery::Done)
    }

    // ---- reaper and maintenance

    /// Recovers jobs whose attempt died, fails what can't finish, creates missing tasks.
    pub async fn reap(&self) -> Result<ReapReport, RepoError> {
        let mut report = ReapReport::default();
        for id in self.repo.expired_leases(100).await? {
            let mut tx = self.repo.pool().begin().await?;
            let Some(job) = Repo::lock_in_state(&mut tx, id, JobState::Processing).await? else {
                continue;
            };
            if job.lease_expires_at.is_some_and(|t| t > chrono::Utc::now()) {
                continue; // renewed by a new claim in the meantime
            }
            if job.attempts >= job.max_attempts {
                let error = JobError {
                    code: codes::RETRIES_EXHAUSTED.into(),
                    detail: None,
                };
                if self.finish_failed_locked(&mut tx, &job, &error).await? {
                    tx.commit().await?;
                    report.failed_exhausted += 1;
                }
            } else if Repo::requeue_expired(&mut tx, id).await? {
                tx.commit().await?;
                report.requeued_expired += 1;
            }
        }
        for id in self.repo.cancelled_queued(100).await? {
            self.cancel_queued(id).await?;
        }
        for id in self
            .repo
            .stale_queued(self.settings.queued_timeout_secs, 100)
            .await?
        {
            let mut tx = self.repo.pool().begin().await?;
            let Some(job) = Repo::lock_in_state(&mut tx, id, JobState::Queued).await? else {
                continue;
            };
            let error = JobError {
                code: codes::TIMED_OUT.into(),
                detail: None,
            };
            if self.finish_failed_locked(&mut tx, &job, &error).await? {
                tx.commit().await?;
                report.timed_out += 1;
            }
        }
        for (id, generation) in self
            .repo
            .take_for_enqueue(self.settings.task_dead_after_secs, 100)
            .await?
        {
            match self.queue.enqueue(id, generation).await {
                Ok(()) => report.enqueued += 1,
                Err(err) => {
                    tracing::warn!(job = %id, %err, "re-enqueue failed");
                    self.repo.mark_needs_enqueue(id).await?;
                }
            }
        }
        let purged = self
            .repo
            .purge_finished(self.settings.retention_days)
            .await?;
        if report != ReapReport::default() || purged > 0 {
            tracing::info!(?report, purged, "reaper");
        }
        Ok(report)
    }

    /// Fails a job the caller locked, with the consumer's hook; false if the hook failed.
    async fn finish_failed_locked(
        &self,
        tx: &mut sqlx::PgConnection,
        job: &Job,
        error: &JobError,
    ) -> Result<bool, RepoError> {
        if let Some(consumer) = self.consumers.get(&job.producer_id) {
            let mut sp = tx.begin().await?;
            if let Err(err) = consumer
                .finalize_failed(&mut sp, job, error, &Value::Null)
                .await
            {
                tracing::warn!(job = %job.id, %err, "consumer failure hook failed");
                return Ok(false);
            }
            sp.commit().await?;
        }
        Repo::finish(tx, job.id, JobState::Failed, None, Some(error)).await?;
        Ok(true)
    }

    pub async fn maintain_consumers(&self) -> Value {
        let mut out = serde_json::Map::new();
        for (producer, consumer) in &self.consumers {
            let value = match consumer.maintain().await {
                Ok(v) => v,
                Err(err) => {
                    tracing::warn!(producer, %err, "consumer maintenance failed");
                    json!({ "error": err })
                }
            };
            out.insert(producer.clone(), value);
        }
        Value::Object(out)
    }

    /// Budget brake: pauses everything once actual spend reaches the budget. Never resumes.
    pub async fn budget_update(&self, cost: f64, budget: f64) -> Result<bool, RepoError> {
        if budget > 0.0 && cost >= budget {
            let reason = format!("budget reached: cost {cost:.2} of {budget:.2}");
            tracing::warn!(%reason, "pausing all ingestion");
            self.repo
                .set_switch("global", true, &reason, "budget-alert")
                .await?;
            return Ok(true);
        }
        Ok(false)
    }
}

async fn file_digest(path: &Path) -> Result<(u64, String), ProcessError> {
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|e| ProcessError::Transient(format!("read output: {e}")))?;
    Ok((bytes.len() as u64, hex(&Sha256::digest(&bytes))))
}
