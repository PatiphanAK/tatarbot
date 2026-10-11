//! Wire-format types for the agent runtime. Kept separate from domain types
//! so the HTTP contract can evolve without touching the core.

use serde::{Deserialize, Serialize};

use crate::domain::{ChatAction, ChatRequest, ChatResponse, ConversationId};

#[derive(Debug, Serialize)]
pub struct ChatRequestDto<'a> {
    pub message: &'a str,
    pub conversation_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_tools: Option<ClientToolsDto<'a>>,
}

#[derive(Debug, Serialize)]
pub struct ClientToolsDto<'a> {
    pub music: MusicToolDto<'a>,
}

#[derive(Debug, Serialize)]
pub struct MusicToolDto<'a> {
    pub stations: Vec<StationDto<'a>>,
}

#[derive(Debug, Serialize)]
pub struct StationDto<'a> {
    pub name: &'a str,
    pub title: &'a str,
}

impl<'a> From<&'a ChatRequest> for ChatRequestDto<'a> {
    fn from(req: &'a ChatRequest) -> Self {
        let stations = &req.client_tools.music_stations;
        Self {
            message: &req.message,
            conversation_id: req.conversation_id.as_str(),
            client_tools: (!stations.is_empty()).then(|| ClientToolsDto {
                music: MusicToolDto {
                    stations: stations
                        .iter()
                        .map(|s| StationDto {
                            name: &s.name,
                            title: &s.title,
                        })
                        .collect(),
                },
            }),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ChatResponseDto {
    pub message: String,
    pub conversation_id: String,
    /// Absent from older runtimes.
    #[serde(default)]
    pub actions: Vec<ActionDto>,
}

#[derive(Debug, Deserialize)]
pub struct ActionDto {
    #[serde(rename = "type")]
    pub kind: String,
    pub action: String,
    #[serde(default)]
    pub station: Option<String>,
}

impl ActionDto {
    /// Unknown kinds or actions are dropped: the bot only does what it knows.
    fn into_action(self) -> Option<ChatAction> {
        match (self.kind.as_str(), self.action.as_str(), self.station) {
            ("play_music", "play", Some(station)) => Some(ChatAction::PlayStation(station)),
            ("play_music", "skip", _) => Some(ChatAction::Skip),
            ("play_music", "stop", _) => Some(ChatAction::Stop),
            ("play_music", "queue", _) => Some(ChatAction::Queue),
            (kind, action, _) => {
                tracing::warn!(kind, action, "ignoring unknown action from the agent");
                None
            }
        }
    }
}

impl TryFrom<ChatResponseDto> for ChatResponse {
    type Error = crate::domain::DomainError;

    fn try_from(dto: ChatResponseDto) -> Result<Self, Self::Error> {
        Ok(ChatResponse {
            message: dto.message,
            conversation_id: ConversationId::new(dto.conversation_id)?,
            actions: dto
                .actions
                .into_iter()
                .filter_map(ActionDto::into_action)
                .collect(),
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
    fn offers_music_stations_only_when_there_are_some() {
        use crate::domain::{ClientTools, StationOffer};
        let req = ChatRequest::new("play lofi", ConversationId::new("c1").unwrap())
            .unwrap()
            .with_client_tools(ClientTools {
                music_stations: vec![StationOffer {
                    name: "lofi".into(),
                    title: "Lo-fi".into(),
                }],
            });
        let json = serde_json::to_value(ChatRequestDto::from(&req)).unwrap();
        assert_eq!(
            json["client_tools"],
            serde_json::json!({"music": {"stations": [{"name": "lofi", "title": "Lo-fi"}]}})
        );
    }

    #[test]
    fn parses_actions_and_drops_unknown_ones() {
        let dto: ChatResponseDto = serde_json::from_str(
            r#"{"message":"","conversation_id":"c1","actions":[
                {"type":"play_music","action":"play","station":"jazz"},
                {"type":"play_music","action":"skip"},
                {"type":"play_music","action":"play"},
                {"type":"play_music","action":"volume_up"},
                {"type":"open_door","action":"now"}
            ]}"#,
        )
        .unwrap();
        let response = ChatResponse::try_from(dto).unwrap();
        assert_eq!(
            response.actions,
            [ChatAction::PlayStation("jazz".into()), ChatAction::Skip]
        );
    }

    #[test]
    fn older_runtimes_without_actions_still_parse() {
        let dto: ChatResponseDto =
            serde_json::from_str(r#"{"message":"hi","conversation_id":"c1"}"#).unwrap();
        assert!(ChatResponse::try_from(dto).unwrap().actions.is_empty());
    }

    #[test]
    fn rejects_response_without_conversation_id() {
        let dto: ChatResponseDto =
            serde_json::from_str(r#"{"message":"x","conversation_id":""}"#).unwrap();
        assert!(ChatResponse::try_from(dto).is_err());
    }
}
