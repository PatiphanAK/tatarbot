//! The processing contract: what producers submit, what the core stores, what consumers and
//! pipelines receive. Tenants and actors are opaque strings chosen by the producer.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// Machine-readable failure codes (`ingest.jobs.error_code`). Consumers may add their own
/// (e.g. `already_in_library`); these are the core's.
pub mod codes {
    pub const POLICY_DENIED: &str = "policy_denied";
    pub const TOO_LARGE: &str = "too_large";
    pub const SOURCE_GONE: &str = "source_gone";
    pub const UNSUPPORTED_SOURCE: &str = "unsupported_source";
    pub const PROCESSING_LIMIT: &str = "processing_limit";
    pub const RETRIES_EXHAUSTED: &str = "retries_exhausted";
    pub const TIMED_OUT: &str = "timed_out";
    pub const INTERNAL: &str = "internal";
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Processing,
    Succeeded,
    Failed,
    Cancelled,
}

impl JobState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Processing => "processing",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "queued" => Self::Queued,
            "processing" => Self::Processing,
            "succeeded" => Self::Succeeded,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            _ => return None,
        })
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    HttpsUrl,
    /// Reserved for upload sessions (Universal Upload, MCP); not processed yet.
    GcsObject,
}

impl SourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HttpsUrl => "https_url",
            Self::GcsObject => "gcs_object",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "https_url" => Some(Self::HttpsUrl),
            "gcs_object" => Some(Self::GcsObject),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    pub kind: SourceKind,
    pub uri: String,
    pub declared_size: Option<u64>,
}

/// `POST /v1/jobs` body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubmitRequest {
    pub idempotency_key: String,
    pub tenant: String,
    pub actor: String,
    pub pipeline: String,
    #[serde(default = "empty_object")]
    pub params: Value,
    pub source: Source,
}

fn empty_object() -> Value {
    Value::Object(Default::default())
}

/// A registered producer and its policy (`ingest.producers`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Producer {
    pub id: String,
    pub service_account: String,
    pub tenant_prefix: String,
    pub allowed_pipelines: Vec<String>,
    pub allowed_source_kinds: Vec<String>,
    pub source_url_hosts: Vec<String>,
    pub max_source_bytes: u64,
    pub max_active_per_actor: u32,
    pub max_jobs_per_tenant_hour: u32,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Job {
    pub id: Uuid,
    pub producer_id: String,
    pub idempotency_key: String,
    pub tenant: String,
    pub actor: String,
    pub pipeline: String,
    pub params: Value,
    pub source: Source,
    pub state: JobState,
    pub attempts: i32,
    pub max_attempts: i32,
    pub lease_owner: Option<Uuid>,
    pub lease_expires_at: Option<DateTime<Utc>>,
    pub cancel_requested: bool,
    pub source_sha256: Option<String>,
    pub result: Option<Value>,
    pub error_code: Option<String>,
    pub error_detail: Option<String>,
    pub enqueued_at: Option<DateTime<Utc>>,
    pub enqueue_generation: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

/// What `GET /v1/jobs/{id}` returns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobView {
    pub job_id: Uuid,
    pub state: JobState,
    pub attempts: i32,
    pub result: Option<Value>,
    pub error_code: Option<String>,
    pub error_detail: Option<String>,
    pub created_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

impl From<&Job> for JobView {
    fn from(job: &Job) -> Self {
        Self {
            job_id: job.id,
            state: job.state,
            attempts: job.attempts,
            result: job.result.clone(),
            error_code: job.error_code.clone(),
            error_detail: job.error_detail.clone(),
            created_at: job.created_at,
            finished_at: job.finished_at,
        }
    }
}

/// A stored output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Artifact {
    /// `gs://bucket/key` (or `file://…` locally).
    pub uri: String,
    /// The object key inside the store.
    pub key: String,
    pub sha256: Option<String>,
    pub size: u64,
    pub media_type: String,
    /// Pipeline-specific, e.g. `{"duration_ms": 183250}`.
    pub metadata: Value,
}

/// `ingest.jobs.result` of a succeeded job.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobResult {
    pub pipeline_version: String,
    pub source_sha256: String,
    /// The consumer reused an existing artifact instead of running the pipeline.
    pub reused: bool,
    pub artifacts: Vec<Artifact>,
    /// What the consumer recorded (e.g. the library track id).
    #[serde(default)]
    pub consumer: Value,
}

/// A terminal failure as recorded and reported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobError {
    pub code: String,
    /// Safe to show to end users (no paths, no stack traces).
    pub detail: Option<String>,
}

/// How a processing step failed; decides retry vs. terminal failure.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ProcessError {
    /// Retrying can't help (bad input, policy, quota): recorded, reported, 2xx.
    #[error("permanent: {code}")]
    Permanent {
        code: String,
        detail: Option<String>,
        /// Extra data for the consumer's failure report (e.g. the existing track).
        data: Value,
    },
    /// Infrastructure hiccup: the attempt is released and Cloud Tasks retries.
    #[error("transient: {0}")]
    Transient(String),
    /// A resource limit (signal, EAGAIN/ENOMEM, timeout): retried once, then permanent.
    #[error("resource limit: {0}")]
    Resource(String),
}

impl ProcessError {
    pub fn permanent(code: &str, detail: impl Into<Option<String>>) -> Self {
        Self::Permanent {
            code: code.to_owned(),
            detail: detail.into(),
            data: Value::Null,
        }
    }
}

/// Lowercase hex of a digest.
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(DIGITS[(b >> 4) as usize] as char);
        out.push(DIGITS[(b & 0xf) as usize] as char);
    }
    out
}
