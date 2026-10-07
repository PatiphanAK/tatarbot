//! Outbound adapter implementing `AgentPort` over HTTP/JSON.
//!
//! Wire contract (see `dto.rs`):
//!
//! ```text
//! POST {base_url}/v1/chat
//! { "message": "...", "conversation_id": "..." }
//! -> 200 { "message": "...", "conversation_id": "..." }
//! ```

mod client;
mod dto;

pub use client::HttpAgentClient;
