//! Music requested in chat ("@TatarBot play some lofi"). The agent runtime
//! returns actions; the bot carries them out through the same `MusicService`
//! as the slash commands, so the same rules apply (you must be in a voice
//! channel, the station must exist, ...). The runtime never calls the bot,
//! and if it is down, slash commands keep working.

use crate::application::ports::{MusicService, PlayContext};
use crate::domain::{ChatAction, ClientTools, StationOffer};

use super::presenter;

/// More would let one message queue up a burst of commands.
pub const MAX_ACTIONS: usize = 2;

/// What the agent may ask for: the configured stations, by name and title.
pub fn offer(music: &dyn MusicService) -> ClientTools {
    ClientTools {
        music_stations: music
            .stations()
            .iter()
            .map(|s| StationOffer {
                name: s.name.clone(),
                title: s.title.clone(),
            })
            .collect(),
    }
}

/// Carries out up to `MAX_ACTIONS`, in order. `Ok` is public text, `Err` a refusal.
pub async fn execute(
    music: &dyn MusicService,
    ctx: PlayContext,
    actions: &[ChatAction],
) -> Vec<Result<String, String>> {
    if actions.len() > MAX_ACTIONS {
        tracing::warn!(requested = actions.len(), "ignoring extra chat actions");
    }
    let mut results = Vec::new();
    for action in actions.iter().take(MAX_ACTIONS) {
        let result = match action {
            ChatAction::PlayStation(name) => music
                .play_station(ctx, name)
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
        results.push(result.map_err(|err| presenter::music_error(&err)));
    }
    results
}

/// The messages to post. The agent's text is kept only if every action
/// succeeded: when one fails it may have claimed success, so only the real
/// outcomes are shown.
pub fn compose(agent_text: &str, results: &[Result<String, String>]) -> Vec<String> {
    let failed = results.iter().any(Result::is_err);
    let mut out = Vec::new();
    if !failed && !agent_text.trim().is_empty() {
        out.push(agent_text.trim().to_owned());
    }
    out.extend(
        results
            .iter()
            .map(|r| r.as_ref().map_or_else(Clone::clone, Clone::clone)),
    );
    out
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
        async fn play(&self, _: PlayContext, _: &str) -> Result<PlayOutcome, MusicError> {
            unreachable!("chat never plays arbitrary queries")
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
        let tools = offer(&FakeMusic::new());
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
}
