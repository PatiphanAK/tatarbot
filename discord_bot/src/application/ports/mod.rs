//! Ports are the only way in and out of the application core.
//!
//! * Inbound (driving) ports are implemented by use cases and called by
//!   inbound adapters such as Discord.
//! * Outbound (driven) ports are implemented by outbound adapters such as
//!   the HTTP agent client and called by use cases.

mod agent;
mod chat;

pub use agent::{AgentError, AgentPort};
pub use chat::{ChatError, ChatService};
