//! Generic asynchronous file ingestion (ADR 0004).
//!
//! A *producer* submits a job (`POST /v1/jobs`) naming a *pipeline* and a source. The engine
//! admits it idempotently, delivers it through a task queue at least once, runs one attempt
//! per delivery under a fenced lease, and commits the outcome together with the producer's
//! *consumer* hooks in one transaction. Nothing here knows about any particular producer:
//! tenants and actors are opaque strings.

pub mod auth;
pub mod engine;
pub mod fetch;
pub mod gcp;
pub mod http;
pub mod migrate;
pub mod model;
pub mod ports;
pub mod repo;

pub use engine::{Delivery, Engine, EngineSettings, SubmitError};
pub use model::{
    Artifact, Job, JobError, JobResult, JobState, JobView, ProcessError, Producer, Source,
    SourceKind, SubmitRequest, codes,
};
pub use ports::{ArtifactStore, Consumer, FetchedDecision, Pipeline, PipelineOutput, TaskQueue};
pub use repo::Repo;
