use async_trait::async_trait;

use crate::application::ports::{AgentPort, ChatError, ChatService};
use crate::domain::{ChatRequest, ChatResponse};

/// Forwards a chat request to the agent and enforces core invariants on the
/// reply. Generic over the agent so it never sees a concrete implementation.
pub struct ChatUseCase<A> {
    agent: A,
}

impl<A: AgentPort> ChatUseCase<A> {
    pub fn new(agent: A) -> Self {
        Self { agent }
    }
}

#[async_trait]
impl<A: AgentPort> ChatService for ChatUseCase<A> {
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, ChatError> {
        let conversation_id = request.conversation_id.clone();
        tracing::debug!(%conversation_id, "forwarding chat request to agent");

        let response = self.agent.chat(request).await?;

        if response.message.trim().is_empty() {
            return Err(ChatError::EmptyReply);
        }
        if response.conversation_id != conversation_id {
            tracing::warn!(
                expected = %conversation_id,
                got = %response.conversation_id,
                "agent replied for a different conversation; keeping the original id"
            );
        }

        Ok(ChatResponse {
            message: response.message,
            conversation_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::application::ports::AgentError;
    use crate::domain::ConversationId;

    /// Test double for the outbound port.
    struct FakeAgent {
        reply: Mutex<Option<Result<ChatResponse, AgentError>>>,
        seen: Mutex<Vec<ChatRequest>>,
    }

    impl FakeAgent {
        fn returning(reply: Result<ChatResponse, AgentError>) -> Self {
            Self {
                reply: Mutex::new(Some(reply)),
                seen: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl AgentPort for FakeAgent {
        async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, AgentError> {
            self.seen.lock().unwrap().push(request);
            self.reply.lock().unwrap().take().expect("called once")
        }
    }

    fn conv(id: &str) -> ConversationId {
        ConversationId::new(id).unwrap()
    }

    fn request() -> ChatRequest {
        ChatRequest::new("hello", conv("c1")).unwrap()
    }

    #[tokio::test]
    async fn returns_agent_reply() {
        let agent = FakeAgent::returning(Ok(ChatResponse {
            message: "hi there".into(),
            conversation_id: conv("c1"),
        }));
        let use_case = ChatUseCase::new(agent);

        let response = use_case.chat(request()).await.unwrap();

        assert_eq!(response.message, "hi there");
        assert_eq!(response.conversation_id, conv("c1"));
        assert_eq!(use_case.agent.seen.lock().unwrap()[0], request());
    }

    #[tokio::test]
    async fn keeps_original_conversation_id() {
        let agent = FakeAgent::returning(Ok(ChatResponse {
            message: "hi".into(),
            conversation_id: conv("other"),
        }));

        let response = ChatUseCase::new(agent).chat(request()).await.unwrap();

        assert_eq!(response.conversation_id, conv("c1"));
    }

    #[tokio::test]
    async fn rejects_empty_reply() {
        let agent = FakeAgent::returning(Ok(ChatResponse {
            message: "  ".into(),
            conversation_id: conv("c1"),
        }));

        let err = ChatUseCase::new(agent).chat(request()).await.unwrap_err();

        assert!(matches!(err, ChatError::EmptyReply));
    }

    #[tokio::test]
    async fn maps_agent_errors() {
        let agent = FakeAgent::returning(Err(AgentError::Timeout));
        let err = ChatUseCase::new(agent).chat(request()).await.unwrap_err();
        assert!(matches!(err, ChatError::Timeout));

        let agent = FakeAgent::returning(Err(AgentError::Unavailable("down".into())));
        let err = ChatUseCase::new(agent).chat(request()).await.unwrap_err();
        assert!(matches!(err, ChatError::AgentUnavailable(_)));
    }
}
