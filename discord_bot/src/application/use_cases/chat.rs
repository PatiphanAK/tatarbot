use async_trait::async_trait;

use super::chat_limits::{ChatLimited, ChatLimiter, ChatPermit, Requester};
use crate::application::ports::{AgentPort, ChatError, ChatService};
use crate::domain::{ChatRequest, ChatResponse};

/// Admits chat turns (rate limits, concurrency), forwards them to the agent and enforces
/// core invariants on the reply. Generic over the agent so it never sees a concrete
/// implementation.
pub struct ChatUseCase<A> {
    agent: A,
    limiter: ChatLimiter,
}

impl<A: AgentPort> ChatUseCase<A> {
    pub fn new(agent: A, limiter: ChatLimiter) -> Self {
        Self { agent, limiter }
    }
}

#[async_trait]
impl<A: AgentPort> ChatService for ChatUseCase<A> {
    async fn admit(&self, requester: Requester) -> Result<ChatPermit, ChatLimited> {
        let admitted = self.limiter.admit(requester).await;
        if let Err(limited) = &admitted {
            tracing::info!(user = requester.user.0, ?limited, "chat turn not admitted");
        }
        admitted
    }

    async fn chat(
        &self,
        request: ChatRequest,
        _permit: ChatPermit,
    ) -> Result<ChatResponse, ChatError> {
        let conversation_id = request.conversation_id.clone();
        tracing::debug!(%conversation_id, "forwarding chat request to agent");

        let response = self.agent.chat(request).await?;

        // An empty message is fine when the agent asked for actions instead.
        if response.message.trim().is_empty() && response.actions.is_empty() {
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
            actions: response.actions,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::application::ports::AgentError;
    use crate::application::use_cases::ChatLimits;
    use crate::domain::ConversationId;
    use crate::domain::music::{GuildId, UserId};

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

    fn use_case(agent: FakeAgent) -> ChatUseCase<FakeAgent> {
        ChatUseCase::new(agent, ChatLimiter::new(ChatLimits::default()))
    }

    const ALICE: Requester = Requester {
        user: UserId(1),
        guild: Some(GuildId(9)),
    };

    async fn turn(uc: &ChatUseCase<FakeAgent>) -> Result<ChatResponse, ChatError> {
        let permit = uc.admit(ALICE).await.expect("admitted");
        uc.chat(request(), permit).await
    }

    /// Agent that answers every call and counts them (the paid model).
    struct CountingAgent(std::sync::atomic::AtomicUsize);

    #[async_trait]
    impl AgentPort for CountingAgent {
        async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, AgentError> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(ChatResponse {
                message: "ok".into(),
                conversation_id: request.conversation_id,
                actions: vec![],
            })
        }
    }

    /// The handler only calls `chat` with a permit from `admit`; a refused message never
    /// reaches the agent, so spam can't run up model or search costs.
    #[tokio::test(start_paused = true)]
    async fn spam_reaches_the_agent_only_for_admitted_turns() {
        let uc = ChatUseCase::new(
            CountingAgent(Default::default()),
            ChatLimiter::new(ChatLimits::default()),
        );
        let mut refused = 0;
        for _ in 0..50 {
            match uc.admit(ALICE).await {
                Ok(permit) => {
                    uc.chat(request(), permit).await.unwrap();
                }
                Err(_) => refused += 1,
            }
        }
        let burst = ChatLimits::default().user_burst as usize;
        assert_eq!(uc.agent.0.load(std::sync::atomic::Ordering::SeqCst), burst);
        assert_eq!(refused, 50 - burst);
    }

    #[tokio::test]
    async fn returns_agent_reply() {
        let agent = FakeAgent::returning(Ok(ChatResponse {
            message: "hi there".into(),
            conversation_id: conv("c1"),
            actions: vec![],
        }));
        let use_case = use_case(agent);

        let response = turn(&use_case).await.unwrap();

        assert_eq!(response.message, "hi there");
        assert_eq!(response.conversation_id, conv("c1"));
        assert_eq!(use_case.agent.seen.lock().unwrap()[0], request());
    }

    #[tokio::test]
    async fn keeps_original_conversation_id() {
        let agent = FakeAgent::returning(Ok(ChatResponse {
            message: "hi".into(),
            conversation_id: conv("other"),
            actions: vec![],
        }));

        let response = turn(&use_case(agent)).await.unwrap();

        assert_eq!(response.conversation_id, conv("c1"));
    }

    #[tokio::test]
    async fn rejects_empty_reply() {
        let agent = FakeAgent::returning(Ok(ChatResponse {
            message: "  ".into(),
            conversation_id: conv("c1"),
            actions: vec![],
        }));

        let err = turn(&use_case(agent)).await.unwrap_err();

        assert!(matches!(err, ChatError::EmptyReply));
    }

    #[tokio::test]
    async fn an_empty_reply_with_actions_is_fine() {
        let agent = FakeAgent::returning(Ok(ChatResponse {
            message: "".into(),
            conversation_id: conv("c1"),
            actions: vec![crate::domain::ChatAction::Skip],
        }));

        let response = turn(&use_case(agent)).await.unwrap();

        assert_eq!(response.actions, [crate::domain::ChatAction::Skip]);
    }

    #[tokio::test]
    async fn maps_agent_errors() {
        let agent = FakeAgent::returning(Err(AgentError::Timeout));
        let err = turn(&use_case(agent)).await.unwrap_err();
        assert!(matches!(err, ChatError::Timeout));

        let agent = FakeAgent::returning(Err(AgentError::Unavailable("down".into())));
        let err = turn(&use_case(agent)).await.unwrap_err();
        assert!(matches!(err, ChatError::AgentUnavailable(_)));
    }
}
