//! TatarBot's Discord adapter as a library: the bot binary (`src/main.rs`) and the
//! ingest worker (`src/bin/ingest_worker.rs`, ADR 0004) compose these modules.

pub mod adapters;
pub mod application;
pub mod config;
pub mod domain;
pub mod error;
pub mod ingest;
pub mod instance_lock;
pub mod library;
#[cfg(test)]
mod test_support;
pub mod tls;
