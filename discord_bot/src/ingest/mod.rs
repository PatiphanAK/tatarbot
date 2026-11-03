//! TatarBot's side of the ingest boundary (ADR 0004). The generic core is the `ingest-core`
//! crate; here are the Discord/music specifics:
//!
//! * `producer`: records `/upload` requests and submits jobs (bot).
//! * `pipeline`: `audio.opus-v1`, the ffmpeg transcode (worker).
//! * `consumer`: library dedupe, quota, track insert and notification, inside the core's
//!   transactions (worker).
//! * `outbox`, `notice`: the transactional notification outbox both sides share.
//! * `gcs`: object stores without HMAC.
//!
//! Discord delivery of notices lives with the Discord adapter
//! (`adapters::inbound::discord::notifications`).

pub mod consumer;
pub mod gcs;
pub mod notice;
pub mod outbox;
pub mod pipeline;
pub mod producer;
