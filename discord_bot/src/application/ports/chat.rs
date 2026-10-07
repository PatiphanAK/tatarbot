use async_trait::async_trait;

use crate::application::ports::AgentError;
use crate::domain::{ChatRequest, ChatResponse};

/// Inbound port: the chat capability the core offers to inbound adapters.
#[async_trait]
pub trait ChatService: Send + Sync {
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, ChatError>;
}

/// Errors surfaced to inbound adapters. Adapters decide how to present them.
#[derive(Debug, thiserror::Error)]
pub enum ChatError {
    #[error("the agent is temporarily unavailable")]
    AgentUnavailable(#[source] AgentError),
    #[error("the agent took too long to respond")]
    Timeout,
    #[error("the request was rejected: {0}")]
    Rejected(String),
    #[error("the agent returned an empty reply")]
    EmptyReply,
    #[error("the agent returned an invalid reply")]
    Internal(#[source] AgentError),
}

impl From<AgentError> for ChatError {
    fn from(err: AgentError) -> Self {
        match err {
            AgentError::Timeout => ChatError::Timeout,
            AgentError::Rejected(reason) => ChatError::Rejected(reason),
            e @ AgentError::Unavailable(_) => ChatError::AgentUnavailable(e),
            e @ AgentError::InvalidResponse(_) => ChatError::Internal(e),
        }
    }
}
