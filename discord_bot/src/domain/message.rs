use super::{ConversationId, DomainError};

/// A user's message addressed to the agent within a conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatRequest {
    pub message: String,
    pub conversation_id: ConversationId,
    /// What the bot can carry out if the agent asks (empty: nothing).
    pub client_tools: ClientTools,
}

/// Client-side tools offered to the agent. The agent never runs them: it returns
/// `ChatAction`s, which the bot validates and carries out itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClientTools {
    /// Radio stations the agent may pick from (empty: no music control).
    pub music_stations: Vec<StationOffer>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StationOffer {
    pub name: String,
    pub title: String,
}

/// Something the agent asked the bot to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatAction {
    PlayStation(String),
    Skip,
    Stop,
    Queue,
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
            client_tools: ClientTools::default(),
        })
    }

    pub fn with_client_tools(mut self, client_tools: ClientTools) -> Self {
        self.client_tools = client_tools;
        self
    }
}

/// The agent's reply within a conversation. `message` may be empty when the
/// agent only asked for actions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatResponse {
    pub message: String,
    pub conversation_id: ConversationId,
    pub actions: Vec<ChatAction>,
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
