//! Music requested in chat ("@TatarBot play some lofi"). The agent runtime
//! returns actions; the bot carries them out through the same `MusicService`
//! as the slash commands, so the same rules apply (you must be in a voice
//! channel, the station must exist, ...). The runtime never calls the bot,
//! and if it is down, slash commands keep working.
//!
//! So the agent remembers what happened, every chat request also carries the
//! current playback state, and the outcomes of the actions carried out after
//! that conversation's previous turn (`PendingReports`).

use std::collections::HashMap;
use std::sync::Mutex;

use crate::application::ports::{MusicService, PlayContext};
use crate::domain::music::{GuildId, VoiceChannelId};
use crate::domain::{
    ActionReport, ChatAction, ClientTools, MusicState, PlayingTrack, StationOffer,
};

use super::presenter;
use crate::adapters::outbound::url_resolver::looks_like_url;

/// More would let one message queue up a burst of commands.
pub const MAX_ACTIONS: usize = 2;
const MAX_DETAIL_CHARS: usize = 300;
/// Bound on conversations with unreported outcomes.
const MAX_PENDING: usize = 1_000;

/// What the agent may ask for (the configured stations; library songs when
/// `library_hints` is `Some`), what is playing now, and how the previous
/// turn's actions turned out.
pub fn offer(
    music: &dyn MusicService,
    state: MusicState,
    results: Vec<ActionReport>,
    library_hints: Option<Vec<String>>,
) -> ClientTools {
    ClientTools {
        music_stations: music
            .stations()
            .iter()
            .map(|s| StationOffer {
                name: s.name.clone(),
                title: s.title.clone(),
            })
            .collect(),
        music_state: Some(state),
        music_results: results,
        music_library: library_hints.is_some(),
        music_library_hints: library_hints.unwrap_or_default(),
    }
}

/// The bot's voice channel (by name) and queue in `guild`.
pub async fn state(
    music: &dyn MusicService,
    guild: GuildId,
    channel_name: impl Fn(VoiceChannelId) -> Option<String>,
) -> MusicState {
    let Some(channel) = music.voice_channel(guild).await else {
        return MusicState::default();
    };
    let queue = music
        .queue(guild)
        .await
        .into_iter()
        .map(|t| PlayingTrack {
            title: t.title,
            live: t.live,
            song: t.now_playing,
        })
        .collect();
    MusicState {
        channel: Some(channel_name(channel).unwrap_or_else(|| "voice".to_owned())),
        queue,
    }
}

pub struct Outcome {
    pub action: ChatAction,
    /// `Ok`: public text; `Err`: a refusal.
    pub result: Result<String, String>,
}

/// Carries out up to `MAX_ACTIONS`, in order.
pub async fn execute(
    music: &dyn MusicService,
    ctx: PlayContext,
    actions: &[ChatAction],
) -> Vec<Outcome> {
    if actions.len() > MAX_ACTIONS {
        tracing::warn!(requested = actions.len(), "ignoring extra chat actions");
    }
    let mut outcomes = Vec::new();
    for action in actions.iter().take(MAX_ACTIONS) {
        let result = match action {
            ChatAction::PlayStation(name) => music
                .play_station(ctx, name)
                .await
                .map(|outcome| presenter::play_outcome(&outcome)),
            // A title for the library search only: /play would fetch a link,
            // and that must never be reachable from chat.
            ChatAction::PlayTrack(title) if looks_like_url(title) => {
                tracing::warn!("refused a link requested through chat");
                outcomes.push(Outcome {
                    action: action.clone(),
                    result: Err(presenter::CHAT_LINK_REFUSED.to_owned()),
                });
                continue;
            }
            ChatAction::PlayTrack(title) => music
                .play(ctx, title)
                .await
                .map(|outcome| presenter::play_outcome(&outcome)),
            ChatAction::Skip => music
                .skip(ctx)
                .await
                .map(|next| presenter::skipped(next.as_ref())),
            ChatAction::Stop => music
                .stop(ctx)
                .await
                .map(|()| presenter::MUSIC_STOPPED.to_owned()),
            ChatAction::Queue => Ok(presenter::queue(&music.queue(ctx.guild).await)),
        };
        tracing::info!(?action, ok = result.is_ok(), "chat action");
        outcomes.push(Outcome {
            action: action.clone(),
            result: result.map_err(|err| presenter::music_error(&err)),
        });
    }
    outcomes
}

/// The messages to post. The agent's text is kept only if every action
/// succeeded: when one fails it may have claimed success, so only the real
/// outcomes are shown.
pub fn compose(agent_text: &str, outcomes: &[Outcome]) -> Vec<String> {
    let failed = outcomes.iter().any(|o| o.result.is_err());
    let mut out = Vec::new();
    if !failed && !agent_text.trim().is_empty() {
        out.push(agent_text.trim().to_owned());
    }
    out.extend(outcomes.iter().map(|o| match &o.result {
        Ok(text) | Err(text) => text.clone(),
    }));
    out
}

/// Outcomes for the agent: what happened, as plain text.
pub fn reports(outcomes: &[Outcome]) -> Vec<ActionReport> {
    outcomes
        .iter()
        .map(|o| {
            let (ok, text) = match &o.result {
                Ok(text) => (true, text),
                Err(text) => (false, text),
            };
            ActionReport {
                action: o.action.clone(),
                ok,
                detail: plain(text),
            }
        })
        .collect()
}

/// Presenter text without Discord markup or mentions.
fn plain(text: &str) -> String {
    let first_line = text.lines().next().unwrap_or_default();
    let without_mention = first_line
        .split(" · requested by")
        .next()
        .unwrap_or_default();
    without_mention
        .replace("**", "")
        .replace('\\', "")
        .chars()
        .take(MAX_DETAIL_CHARS)
        .collect()
}

/// Outcomes waiting for the conversation's next chat request.
#[derive(Default)]
pub struct PendingReports(Mutex<HashMap<String, Vec<ActionReport>>>);

impl PendingReports {
    pub fn put(&self, conversation: &str, reports: Vec<ActionReport>) {
        if reports.is_empty() {
            return;
        }
        let mut pending = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if pending.len() >= MAX_PENDING {
            pending.clear();
        }
        pending
            .entry(conversation.to_owned())
            .or_default()
            .extend(reports);
    }

    pub fn take(&self, conversation: &str) -> Vec<ActionReport> {
        let mut pending = self.0.lock().unwrap_or_else(|e| e.into_inner());
        pending.remove(conversation).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;

    use super::*;
    use crate::application::ports::{MusicError, PlayOutcome};
    use crate::domain::music::{GuildId, StationList, TrackInfo, UserId, VoiceChannelId};

    /// Records calls; playing needs a voice channel and a known station.
    struct FakeMusic {
        stations: StationList,
        calls: Mutex<Vec<String>>,
    }

    impl FakeMusic {
        fn new() -> Self {
            Self {
                stations: StationList::from_toml(
                    "[[station]]\nname = \"lofi\"\ntitle = \"Lo-fi beats\"\nurl = \"https://r.example/l\"\n",
                )
                .unwrap(),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    fn track(title: &str) -> TrackInfo {
        TrackInfo {
            title: title.into(),
            live: true,
            requested_by: UserId(7),
            now_playing: None,
        }
    }

    #[async_trait]
    impl MusicService for FakeMusic {
        async fn play(&self, ctx: PlayContext, query: &str) -> Result<PlayOutcome, MusicError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("play_track {query}"));
            ctx.user_channel.ok_or(MusicError::NotInVoice)?;
            if query.to_lowercase().contains("boys") {
                Ok(PlayOutcome::Queued {
                    track: TrackInfo {
                        live: false,
                        ..track("Boys Dont Cry")
                    },
                    position: 1,
                })
            } else {
                Err(MusicError::Resolve(
                    crate::application::ports::ResolveError::NotFound(query.into()),
                ))
            }
        }
        async fn play_station(
            &self,
            ctx: PlayContext,
            name: &str,
        ) -> Result<PlayOutcome, MusicError> {
            self.calls.lock().unwrap().push(format!("play {name}"));
            ctx.user_channel.ok_or(MusicError::NotInVoice)?;
            let station = self
                .stations
                .get(name)
                .ok_or_else(|| MusicError::UnknownStation(name.into()))?;
            Ok(PlayOutcome::NowPlaying(track(&station.title)))
        }
        async fn skip(&self, _: PlayContext) -> Result<Option<TrackInfo>, MusicError> {
            self.calls.lock().unwrap().push("skip".into());
            Ok(None)
        }
        async fn stop(&self, _: PlayContext) -> Result<(), MusicError> {
            self.calls.lock().unwrap().push("stop".into());
            Err(MusicError::NothingPlaying)
        }
        async fn leave(&self, _: PlayContext) -> Result<(), MusicError> {
            unreachable!("chat can't make the bot leave")
        }
        async fn queue(&self, _: GuildId) -> Vec<TrackInfo> {
            vec![track("Lo-fi beats")]
        }
        async fn voice_channel(&self, _: GuildId) -> Option<VoiceChannelId> {
            Some(VoiceChannelId(10))
        }
        fn stations(&self) -> &StationList {
            &self.stations
        }
    }

    fn in_voice() -> PlayContext {
        PlayContext {
            guild: GuildId(1),
            user: UserId(7),
            user_channel: Some(VoiceChannelId(10)),
        }
    }

    #[test]
    fn offers_the_configured_stations() {
        let tools = offer(&FakeMusic::new(), MusicState::default(), Vec::new(), None);
        assert!(!tools.music_library);
        assert_eq!(
            tools.music_stations,
            [StationOffer {
                name: "lofi".into(),
                title: "Lo-fi beats".into()
            }]
        );
    }

    #[tokio::test]
    async fn plays_through_the_music_service() {
        let music = FakeMusic::new();

        let results = execute(
            &music,
            in_voice(),
            &[ChatAction::PlayStation("lofi".into())],
        )
        .await;

        assert_eq!(*music.calls.lock().unwrap(), ["play lofi"]);
        assert_eq!(compose("Sure! 🎶", &results)[0], "Sure! 🎶");
        assert!(compose("Sure! 🎶", &results)[1].starts_with("▶️ Now playing **Lo-fi beats**"));
    }

    #[tokio::test]
    async fn the_music_rules_apply_and_failures_replace_the_agents_text() {
        let music = FakeMusic::new();
        let not_in_voice = PlayContext {
            user_channel: None,
            ..in_voice()
        };

        let results = execute(
            &music,
            not_in_voice,
            &[ChatAction::PlayStation("lofi".into())],
        )
        .await;

        assert_eq!(
            compose("Now playing lofi!", &results),
            ["🔇 Join a voice channel first."]
        );
        let unknown = execute(
            &music,
            in_voice(),
            &[ChatAction::PlayStation("polka".into())],
        )
        .await;
        assert!(compose("", &unknown)[0].contains("`polka`"));
    }

    #[tokio::test]
    async fn at_most_two_actions_run() {
        let music = FakeMusic::new();
        let actions = [ChatAction::Skip, ChatAction::Skip, ChatAction::Skip];

        let results = execute(&music, in_voice(), &actions).await;

        assert_eq!(results.len(), MAX_ACTIONS);
        assert_eq!(music.calls.lock().unwrap().len(), MAX_ACTIONS);
    }

    #[tokio::test]
    async fn every_action_maps_to_a_reply() {
        let music = FakeMusic::new();
        let results = execute(&music, in_voice(), &[ChatAction::Queue, ChatAction::Stop]).await;

        let posted = compose("ok", &results);
        assert!(posted[0].starts_with("▶️ **Lo-fi beats**"), "{posted:?}");
        assert_eq!(posted[1], "📭 Nothing is playing.");
    }

    #[tokio::test]
    async fn outcomes_become_plain_reports() {
        let music = FakeMusic::new();
        let not_in_voice = PlayContext {
            user_channel: None,
            ..in_voice()
        };
        let mut outcomes = execute(
            &music,
            in_voice(),
            &[ChatAction::PlayStation("lofi".into())],
        )
        .await;
        outcomes.extend(
            execute(
                &music,
                not_in_voice,
                &[ChatAction::PlayStation("lofi".into())],
            )
            .await,
        );

        let reports = reports(&outcomes);

        assert_eq!(
            reports[0],
            ActionReport {
                action: ChatAction::PlayStation("lofi".into()),
                ok: true,
                detail: "▶️ Now playing Lo-fi beats 🔴 LIVE".into(),
            }
        );
        assert!(!reports[1].ok);
        assert_eq!(reports[1].detail, "🔇 Join a voice channel first.");
    }

    #[test]
    fn pending_reports_wait_for_their_conversation() {
        let pending = PendingReports::default();
        let report = ActionReport {
            action: ChatAction::Skip,
            ok: true,
            detail: "Skipped".into(),
        };
        pending.put("a", vec![report.clone()]);
        pending.put("a", vec![report.clone()]);
        pending.put("b", Vec::new());

        assert!(pending.take("b").is_empty());
        assert_eq!(pending.take("a"), [report.clone(), report]);
        assert!(pending.take("a").is_empty(), "taken once");
    }

    #[tokio::test]
    async fn state_reports_channel_and_queue() {
        let music = FakeMusic::new();

        let state = state(&music, GuildId(1), |c| Some(format!("room-{}", c.0))).await;

        assert_eq!(state.channel.as_deref(), Some("room-10"));
        assert_eq!(state.queue[0].title, "Lo-fi beats");
        assert!(state.queue[0].live);
    }

    #[test]
    fn offers_the_library_with_hints() {
        let hints = vec!["Boys Dont Cry".to_owned()];
        let tools = offer(
            &FakeMusic::new(),
            MusicState::default(),
            Vec::new(),
            Some(hints),
        );
        assert!(tools.music_library);
        assert_eq!(tools.music_library_hints, ["Boys Dont Cry"]);
    }

    #[tokio::test]
    async fn songs_are_found_in_the_library() {
        let music = FakeMusic::new();

        let outcomes = execute(&music, in_voice(), &[ChatAction::PlayTrack("boys".into())]).await;

        assert_eq!(*music.calls.lock().unwrap(), ["play_track boys"]);
        assert!(compose("", &outcomes)[0].starts_with("➕ Queued #1: **Boys Dont Cry**"));
        let report = &reports(&outcomes)[0];
        assert!(report.ok && report.detail.starts_with("➕ Queued #1: Boys Dont Cry"));

        let missing = execute(&music, in_voice(), &[ChatAction::PlayTrack("xyz".into())]).await;
        assert!(compose("", &missing)[0].contains("Nothing in this server's library matches"));
    }

    #[tokio::test]
    async fn links_are_never_played_from_chat() {
        let music = FakeMusic::new();
        let link = ChatAction::PlayTrack("https://evil.example/a.mp3".into());

        let outcomes = execute(&music, in_voice(), &[link]).await;

        assert!(
            music.calls.lock().unwrap().is_empty(),
            "music.play must not be called"
        );
        assert_eq!(compose("", &outcomes), [presenter::CHAT_LINK_REFUSED]);
        assert!(!reports(&outcomes)[0].ok);
    }
}
