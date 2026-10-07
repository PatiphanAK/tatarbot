use async_trait::async_trait;

use crate::domain::{ChatRequest, ChatResponse};

/// Outbound port to the Agent Runtime (or a test fake). The core never
/// knows how the agent is implemented or which model it uses.
#[async_trait]
pub trait AgentPort: Send + Sync {
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, AgentError>;
}

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    /// The agent could not be reached or is temporarily failing.
    #[error("agent unavailable: {0}")]
    Unavailable(String),
    /// The agent did not answer in time.
    #[error("agent timed out")]
    Timeout,
    /// The agent refused the request (bad input, policy, quota...).
    #[error("agent rejected request: {0}")]
    Rejected(String),
    /// The agent answered with something we could not understand.
    #[error("invalid agent response: {0}")]
    InvalidResponse(String),
}
