//! Framework-independent domain types. Nothing in here may depend on
//! Discord, HTTP, or any concrete agent runtime.

mod conversation;
pub mod library;
mod message;
pub mod music;

pub use conversation::ConversationId;
pub use message::{ChatRequest, ChatResponse};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DomainError {
    #[error("conversation id must not be empty")]
    EmptyConversationId,
    #[error("message must not be empty")]
    EmptyMessage,
}
