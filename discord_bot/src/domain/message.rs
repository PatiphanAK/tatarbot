use super::{ConversationId, DomainError};

/// A user's message addressed to the agent within a conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatRequest {
    pub message: String,
    pub conversation_id: ConversationId,
}

impl ChatRequest {
    /// Builds a request, trimming surrounding whitespace and rejecting
    /// empty messages.
    pub fn new(
        message: impl Into<String>,
        conversation_id: ConversationId,
    ) -> Result<Self, DomainError> {
        let message = message.into().trim().to_owned();
        if message.is_empty() {
            return Err(DomainError::EmptyMessage);
        }
        Ok(Self {
            message,
            conversation_id,
        })
    }
}

/// The agent's reply within a conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatResponse {
    pub message: String,
    pub conversation_id: ConversationId,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conv() -> ConversationId {
        ConversationId::new("c1").unwrap()
    }

    #[test]
    fn trims_message() {
        assert_eq!(ChatRequest::new("  hi \n", conv()).unwrap().message, "hi");
    }

    #[test]
    fn rejects_blank_message() {
        assert_eq!(
            ChatRequest::new(" \n ", conv()),
            Err(DomainError::EmptyMessage)
        );
    }
}
