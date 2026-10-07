//! Wire-format types for the agent runtime. Kept separate from domain types
//! so the HTTP contract can evolve without touching the core.

use serde::{Deserialize, Serialize};

use crate::domain::{ChatRequest, ChatResponse, ConversationId};

#[derive(Debug, Serialize)]
pub struct ChatRequestDto<'a> {
    pub message: &'a str,
    pub conversation_id: &'a str,
}

impl<'a> From<&'a ChatRequest> for ChatRequestDto<'a> {
    fn from(req: &'a ChatRequest) -> Self {
        Self {
            message: &req.message,
            conversation_id: req.conversation_id.as_str(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ChatResponseDto {
    pub message: String,
    pub conversation_id: String,
}

impl TryFrom<ChatResponseDto> for ChatResponse {
    type Error = crate::domain::DomainError;

    fn try_from(dto: ChatResponseDto) -> Result<Self, Self::Error> {
        Ok(ChatResponse {
            message: dto.message,
            conversation_id: ConversationId::new(dto.conversation_id)?,
        })
    }
}

/// Optional error body: `{ "error": "..." }`.
#[derive(Debug, Deserialize)]
pub struct ErrorDto {
    pub error: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_request() {
        let req = ChatRequest::new("hi", ConversationId::new("c1").unwrap()).unwrap();
        let json = serde_json::to_value(ChatRequestDto::from(&req)).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"message": "hi", "conversation_id": "c1"})
        );
    }

    #[test]
    fn rejects_response_without_conversation_id() {
        let dto: ChatResponseDto =
            serde_json::from_str(r#"{"message":"x","conversation_id":""}"#).unwrap();
        assert!(ChatResponse::try_from(dto).is_err());
    }
}
