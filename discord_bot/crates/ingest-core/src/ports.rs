//! Extension points. Pipelines turn an input file into an output; consumers apply a job's
//! outcome to their own domain inside the core's transactions; artifact stores and task
//! queues are infrastructure.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::model::{Artifact, Job, JobError, JobResult, ProcessError};

/// One output file of a pipeline run.
#[derive(Debug, Clone, PartialEq)]
pub struct PipelineOutput {
    pub path: PathBuf,
    pub media_type: String,
    /// File extension for the default object key (`"ogg"`).
    pub extension: String,
    pub metadata: Value,
}

#[async_trait]
pub trait Pipeline: Send + Sync {
    /// Recorded in results; bump it when the output for the same input changes.
    fn version(&self) -> &str;
    /// Rejects bad `params` at submission time (policy error).
    fn validate_params(&self, params: &Value) -> Result<(), String>;
    async fn run(
        &self,
        input: &Path,
        out_dir: &Path,
        params: &Value,
    ) -> Result<PipelineOutput, ProcessError>;
}

/// What a consumer wants after the source is fetched and hashed.
#[derive(Debug, Clone, PartialEq)]
pub enum FetchedDecision {
    Continue,
    /// An existing artifact already holds this content: skip the pipeline.
    Reuse(Artifact),
    Reject(ProcessError),
}

/// A producer's in-process consumer (compiled into the worker). All `finalize_*` hooks run in
/// the transaction that also changes the job's state, so domain effects happen exactly once.
#[async_trait]
pub trait Consumer: Send + Sync {
    async fn on_fetched(&self, _job: &Job, _sha256: &str) -> Result<FetchedDecision, ProcessError> {
        Ok(FetchedDecision::Continue)
    }

    /// Object key for the pipeline's output; `None` = the core's default
    /// (`<producer>/<sha256>.<ext>`).
    fn output_key(&self, _job: &Job, _sha256: &str, _extension: &str) -> Option<String> {
        None
    }

    /// Applies a success. Returning `Err(Permanent)` rolls the transaction back and fails the
    /// job instead (e.g. quota exceeded); `Err(Transient)` releases it for a retry.
    async fn finalize_succeeded(
        &self,
        tx: &mut PgConnection,
        job: &Job,
        result: &JobResult,
    ) -> Result<Value, ProcessError>;

    async fn finalize_failed(
        &self,
        tx: &mut PgConnection,
        job: &Job,
        error: &JobError,
        data: &Value,
    ) -> Result<(), ProcessError>;

    async fn finalize_cancelled(
        &self,
        tx: &mut PgConnection,
        job: &Job,
    ) -> Result<(), ProcessError>;

    /// Periodic housekeeping (`/internal/maintenance/consumers`), e.g. orphan sweeps.
    async fn maintain(&self) -> Result<Value, String> {
        Ok(Value::Null)
    }
}

/// Where outputs go. Writes are create-only, so a retry never replaces committed data.
#[async_trait]
pub trait ArtifactStore: Send + Sync {
    /// Uploads `path` to `key` unless it already exists; returns the artifact's URI.
    async fn put_if_absent(
        &self,
        key: &str,
        path: &Path,
        media_type: &str,
    ) -> Result<String, ProcessError>;
}

/// Delivers "process this job" at least once.
#[async_trait]
pub trait TaskQueue: Send + Sync {
    /// Idempotent per (job, generation).
    async fn enqueue(&self, job: Uuid, generation: i32) -> Result<(), String>;
}
