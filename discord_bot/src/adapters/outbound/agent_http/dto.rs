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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<MusicStateDto<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub results: Vec<ActionResultDto<'a>>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub library: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub library_hints: Vec<&'a str>,
}

#[derive(Debug, Serialize)]
pub struct MusicStateDto<'a> {
    pub channel: Option<&'a str>,
    pub queue: Vec<PlayingTrackDto<'a>>,
}

#[derive(Debug, Serialize)]
pub struct PlayingTrackDto<'a> {
    pub title: &'a str,
    pub live: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub song: Option<&'a str>,
}

#[derive(Debug, Serialize)]
pub struct ActionResultDto<'a> {
    pub action: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub station: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track: Option<&'a str>,
    pub ok: bool,
    pub detail: &'a str,
}

/// The runtime accepts at most this many tracks in the state.
const MAX_STATE_TRACKS: usize = 11;
/// ... this many results, and this many library hints.
const MAX_RESULTS: usize = 4;
const MAX_HINTS: usize = 20;

/// At most `n` characters of `s`, cut on a character boundary.
fn first_chars(s: &str, n: usize) -> &str {
    s.char_indices().nth(n).map_or(s, |(i, _)| &s[..i])
}

/// (action, station, track) as the runtime names them.
fn action_fields(action: &ChatAction) -> (&'static str, Option<&str>, Option<&str>) {
    match action {
        ChatAction::PlayStation(station) => ("play", Some(station.as_str()), None),
        ChatAction::PlayTrack(track) => ("play_track", None, Some(track.as_str())),
        ChatAction::Skip => ("skip", None, None),
        ChatAction::Stop => ("stop", None, None),
        ChatAction::Queue => ("queue", None, None),
    }
}

#[derive(Debug, Serialize)]
pub struct StationDto<'a> {
    pub name: &'a str,
    pub title: &'a str,
}

impl<'a> From<&'a ChatRequest> for ChatRequestDto<'a> {
    fn from(req: &'a ChatRequest) -> Self {
        let tools = &req.client_tools;
        let stations = &tools.music_stations;
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
                    state: tools.music_state.as_ref().map(|state| MusicStateDto {
                        channel: state.channel.as_deref(),
                        queue: state
                            .queue
                            .iter()
                            .take(MAX_STATE_TRACKS)
                            .map(|t| PlayingTrackDto {
                                title: &t.title,
                                live: t.live,
                                song: t.song.as_deref(),
                            })
                            .collect(),
                    }),
                    results: tools
                        .music_results
                        .iter()
                        .take(MAX_RESULTS)
                        .map(|r| {
                            let (action, station, track) = action_fields(&r.action);
                            ActionResultDto {
                                action,
                                station,
                                track,
                                ok: r.ok,
                                detail: &r.detail,
                            }
                        })
                        .collect(),
                    library: tools.music_library,
                    library_hints: tools
                        .music_library_hints
                        .iter()
                        .take(MAX_HINTS)
                        .map(|t| first_chars(t, 100))
                        .filter(|t| !t.is_empty())
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
    #[serde(default)]
    pub track: Option<String>,
}

impl ActionDto {
    /// Unknown kinds or actions are dropped: the bot only does what it knows.
    fn into_action(self) -> Option<ChatAction> {
        match (
            self.kind.as_str(),
            self.action.as_str(),
            self.station,
            self.track,
        ) {
            ("play_music", "play", Some(station), _) => Some(ChatAction::PlayStation(station)),
            ("play_music", "play_track", _, Some(track)) => Some(ChatAction::PlayTrack(track)),
            ("play_music", "skip", ..) => Some(ChatAction::Skip),
            ("play_music", "stop", ..) => Some(ChatAction::Stop),
            ("play_music", "queue", ..) => Some(ChatAction::Queue),
            (kind, action, ..) => {
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
                ..ClientTools::default()
            });
        assert!(
            serde_json::to_value(ChatRequestDto::from(&req)).unwrap()["client_tools"]["music"]
                .get("library")
                .is_none(),
            "no library field when there is no library"
        );
        let json = serde_json::to_value(ChatRequestDto::from(&req)).unwrap();
        assert_eq!(
            json["client_tools"],
            serde_json::json!({"music": {"stations": [{"name": "lofi", "title": "Lo-fi"}]}})
        );
    }

    #[test]
    fn sends_playback_state_and_outcomes() {
        use crate::domain::{ActionReport, ClientTools, MusicState, PlayingTrack, StationOffer};
        let req = ChatRequest::new("what's on?", ConversationId::new("c1").unwrap())
            .unwrap()
            .with_client_tools(ClientTools {
                music_stations: vec![StationOffer {
                    name: "lofi".into(),
                    title: "Lo-fi".into(),
                }],
                music_state: Some(MusicState {
                    channel: Some("General".into()),
                    queue: vec![PlayingTrack {
                        title: "Lo-fi".into(),
                        live: true,
                        song: Some("A - B".into()),
                    }],
                }),
                music_results: vec![
                    ActionReport {
                        action: ChatAction::PlayStation("lofi".into()),
                        ok: true,
                        detail: "Now playing Lo-fi".into(),
                    },
                    ActionReport {
                        action: ChatAction::PlayTrack("boys".into()),
                        ok: false,
                        detail: "Nothing matches".into(),
                    },
                ],
                music_library: true,
                music_library_hints: vec!["Boys Dont Cry".into(), "ก".repeat(150)],
            });
        let json = serde_json::to_value(ChatRequestDto::from(&req)).unwrap();
        assert_eq!(
            json["client_tools"]["music"]["state"],
            serde_json::json!({"channel": "General", "queue": [{"title": "Lo-fi", "live": true, "song": "A - B"}]})
        );
        assert_eq!(
            json["client_tools"]["music"]["results"],
            serde_json::json!([
                {"action": "play", "station": "lofi", "ok": true, "detail": "Now playing Lo-fi"},
                {"action": "play_track", "track": "boys", "ok": false, "detail": "Nothing matches"}
            ])
        );
        let hints = json["client_tools"]["music"]["library_hints"]
            .as_array()
            .unwrap();
        assert_eq!(json["client_tools"]["music"]["library"], true);
        assert_eq!(hints[0], "Boys Dont Cry");
        assert_eq!(
            hints[1].as_str().unwrap().chars().count(),
            100,
            "cut, not split mid-character"
        );
    }

    #[test]
    fn parses_actions_and_drops_unknown_ones() {
        let dto: ChatResponseDto = serde_json::from_str(
            r#"{"message":"","conversation_id":"c1","actions":[
                {"type":"play_music","action":"play","station":"jazz"},
                {"type":"play_music","action":"skip"},
                {"type":"play_music","action":"play_track","track":"Boys Dont Cry"},
                {"type":"play_music","action":"play_track"},
                {"type":"play_music","action":"play"},
                {"type":"play_music","action":"volume_up"},
                {"type":"open_door","action":"now"}
            ]}"#,
        )
        .unwrap();
        let response = ChatResponse::try_from(dto).unwrap();
        assert_eq!(
            response.actions,
            [
                ChatAction::PlayStation("jazz".into()),
                ChatAction::Skip,
                ChatAction::PlayTrack("Boys Dont Cry".into())
            ]
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
