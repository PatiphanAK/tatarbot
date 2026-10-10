use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::application::ports::{
    MusicError, MusicService, PlayContext, PlayOutcome, TrackResolver, VoicePort,
};
use crate::domain::music::{GuildId, StationList, Track, TrackInfo, VoiceChannelId};

/// All playback rules, whoever the caller is:
///
/// * Playing needs the caller in a voice channel. The bot joins it, or moves
///   to it when idle; it never leaves listeners mid-track for another channel.
/// * Controlling playback (skip, stop, leave) needs the caller in the bot's
///   channel.
/// * Live streams never end, so anything queued behind one replaces it, and
///   `/radio` replaces the whole queue.
/// * The queue is capped.
///
/// Operations on one guild are serialised, so concurrent commands can't race
/// a join against an enqueue.
pub struct MusicUseCase<V, R> {
    voice: V,
    resolver: R,
    stations: StationList,
    max_queue: usize,
    locks: Mutex<HashMap<GuildId, Arc<tokio::sync::Mutex<()>>>>,
}

#[derive(Clone, Copy)]
enum Mode {
    Append,
    Replace,
}

impl<V: VoicePort, R: TrackResolver> MusicUseCase<V, R> {
    pub fn new(voice: V, resolver: R, stations: StationList, max_queue: usize) -> Self {
        Self {
            voice,
            resolver,
            stations,
            max_queue: max_queue.max(1),
            locks: Mutex::new(HashMap::new()),
        }
    }

    fn lock_for(&self, guild: GuildId) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.locks.lock().unwrap_or_else(|e| e.into_inner());
        locks.entry(guild).or_default().clone()
    }

    /// Joins (or moves to) the caller's channel unless that would interrupt
    /// listeners elsewhere.
    async fn ensure_joined(&self, ctx: PlayContext) -> Result<(), MusicError> {
        let target = ctx.user_channel.ok_or(MusicError::NotInVoice)?;
        match self.voice.current_channel(ctx.guild).await {
            Some(current) if current == target => Ok(()),
            Some(current) if !self.voice.queue(ctx.guild).await.is_empty() => {
                Err(MusicError::BusyElsewhere(current))
            }
            _ => Ok(self.voice.join(ctx.guild, target).await?),
        }
    }

    async fn start(
        &self,
        ctx: PlayContext,
        track: Track,
        mode: Mode,
    ) -> Result<PlayOutcome, MusicError> {
        self.ensure_joined(ctx).await?;
        let queue = self.voice.queue(ctx.guild).await;
        let replace_live = queue.first().is_some_and(|t| t.live);
        match mode {
            Mode::Replace if !queue.is_empty() => self.voice.stop(ctx.guild).await?,
            Mode::Append if queue.len() >= self.max_queue => {
                return Err(MusicError::QueueFull(self.max_queue));
            }
            _ => {}
        }

        let info = track.info.clone();
        let mut position = self.voice.enqueue(ctx.guild, track).await?;
        if matches!(mode, Mode::Append) && replace_live && position > 0 {
            // A live stream would never yield to the queue.
            self.voice.skip(ctx.guild).await?;
            position -= 1;
        }
        Ok(match position {
            0 => PlayOutcome::NowPlaying(info),
            position => PlayOutcome::Queued {
                track: info,
                position,
            },
        })
    }

    /// The bot's channel, provided the caller is in it.
    async fn require_listener(&self, ctx: PlayContext) -> Result<VoiceChannelId, MusicError> {
        let channel = self
            .voice
            .current_channel(ctx.guild)
            .await
            .ok_or(MusicError::NothingPlaying)?;
        if ctx.user_channel != Some(channel) {
            return Err(MusicError::NotInBotChannel(channel));
        }
        Ok(channel)
    }
}

#[async_trait]
impl<V: VoicePort, R: TrackResolver> MusicService for MusicUseCase<V, R> {
    async fn play(&self, ctx: PlayContext, query: &str) -> Result<PlayOutcome, MusicError> {
        if ctx.user_channel.is_none() {
            return Err(MusicError::NotInVoice);
        }
        // Resolve before joining: a bad link should not pull the bot in.
        let track = self.resolver.resolve(ctx.guild, ctx.user, query).await?;
        let lock = self.lock_for(ctx.guild);
        let _guard = lock.lock().await;
        self.start(ctx, track, Mode::Append).await
    }

    async fn play_station(&self, ctx: PlayContext, name: &str) -> Result<PlayOutcome, MusicError> {
        if ctx.user_channel.is_none() {
            return Err(MusicError::NotInVoice);
        }
        let station = self
            .stations
            .get(name)
            .ok_or_else(|| MusicError::UnknownStation(name.trim().to_owned()))?;
        let track = Track::from_station(station, ctx.user);
        let lock = self.lock_for(ctx.guild);
        let _guard = lock.lock().await;
        self.start(ctx, track, Mode::Replace).await
    }

    async fn skip(&self, ctx: PlayContext) -> Result<Option<TrackInfo>, MusicError> {
        let lock = self.lock_for(ctx.guild);
        let _guard = lock.lock().await;
        self.require_listener(ctx).await?;
        let queue = self.voice.queue(ctx.guild).await;
        if queue.is_empty() {
            return Err(MusicError::NothingPlaying);
        }
        self.voice.skip(ctx.guild).await?;
        Ok(queue.into_iter().nth(1))
    }

    async fn stop(&self, ctx: PlayContext) -> Result<(), MusicError> {
        let lock = self.lock_for(ctx.guild);
        let _guard = lock.lock().await;
        self.require_listener(ctx).await?;
        if self.voice.queue(ctx.guild).await.is_empty() {
            return Err(MusicError::NothingPlaying);
        }
        Ok(self.voice.stop(ctx.guild).await?)
    }

    async fn leave(&self, ctx: PlayContext) -> Result<(), MusicError> {
        let lock = self.lock_for(ctx.guild);
        let _guard = lock.lock().await;
        self.require_listener(ctx).await?;
        Ok(self.voice.leave(ctx.guild).await?)
    }

    async fn queue(&self, guild: GuildId) -> Vec<TrackInfo> {
        self.voice.queue(guild).await
    }

    fn stations(&self) -> &StationList {
        &self.stations
    }
}

#[cfg(test)]
mod tests {
    use url::Url;

    use super::*;
    use crate::application::ports::{ResolveError, VoiceError};
    use crate::domain::music::{TrackSource, UserId};

    const GUILD: GuildId = GuildId(1);
    const ROOM: VoiceChannelId = VoiceChannelId(10);
    const OTHER_ROOM: VoiceChannelId = VoiceChannelId(11);
    const ALICE: UserId = UserId(100);

    /// In-memory voice connection: a channel per guild and a queue.
    #[derive(Default)]
    struct FakeVoice {
        state: Mutex<HashMap<GuildId, (VoiceChannelId, Vec<TrackInfo>)>>,
        calls: Mutex<Vec<String>>,
    }

    impl FakeVoice {
        fn log(&self, call: impl Into<String>) {
            self.calls.lock().unwrap().push(call.into());
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl VoicePort for Arc<FakeVoice> {
        async fn current_channel(&self, guild: GuildId) -> Option<VoiceChannelId> {
            self.state.lock().unwrap().get(&guild).map(|(c, _)| *c)
        }
        async fn join(&self, guild: GuildId, channel: VoiceChannelId) -> Result<(), VoiceError> {
            self.log(format!("join {}", channel.0));
            let mut state = self.state.lock().unwrap();
            state.entry(guild).or_insert((channel, Vec::new())).0 = channel;
            Ok(())
        }
        async fn enqueue(&self, guild: GuildId, track: Track) -> Result<usize, VoiceError> {
            self.log(format!("enqueue {}", track.info.title));
            let mut state = self.state.lock().unwrap();
            let (_, queue) = state.get_mut(&guild).ok_or(VoiceError::NotConnected)?;
            queue.push(track.info);
            Ok(queue.len() - 1)
        }
        async fn queue(&self, guild: GuildId) -> Vec<TrackInfo> {
            let state = self.state.lock().unwrap();
            state
                .get(&guild)
                .map(|(_, q)| q.clone())
                .unwrap_or_default()
        }
        async fn skip(&self, guild: GuildId) -> Result<(), VoiceError> {
            self.log("skip");
            let mut state = self.state.lock().unwrap();
            let (_, queue) = state.get_mut(&guild).ok_or(VoiceError::NotConnected)?;
            if !queue.is_empty() {
                queue.remove(0);
            }
            Ok(())
        }
        async fn stop(&self, guild: GuildId) -> Result<(), VoiceError> {
            self.log("stop");
            let mut state = self.state.lock().unwrap();
            state
                .get_mut(&guild)
                .ok_or(VoiceError::NotConnected)?
                .1
                .clear();
            Ok(())
        }
        async fn leave(&self, guild: GuildId) -> Result<(), VoiceError> {
            self.log("leave");
            self.state.lock().unwrap().remove(&guild);
            Ok(())
        }
    }

    /// Accepts `https://…` queries as finite tracks named after the URL path.
    struct FakeResolver;

    #[async_trait]
    impl TrackResolver for FakeResolver {
        async fn resolve(
            &self,
            _: GuildId,
            requested_by: UserId,
            query: &str,
        ) -> Result<Track, ResolveError> {
            let url = Url::parse(query).map_err(|_| ResolveError::SearchUnavailable)?;
            Ok(Track {
                info: TrackInfo {
                    title: url.path().trim_matches('/').to_owned(),
                    live: false,
                    requested_by,
                    now_playing: None,
                },
                source: TrackSource::Http(url),
            })
        }
    }

    fn stations() -> StationList {
        StationList::from_toml(
            "[[station]]\nname = \"lofi\"\ntitle = \"Lofi\"\nurl = \"https://r.example/lofi\"\n",
        )
        .unwrap()
    }

    fn music(max_queue: usize) -> (MusicUseCase<Arc<FakeVoice>, FakeResolver>, Arc<FakeVoice>) {
        let voice = Arc::new(FakeVoice::default());
        (
            MusicUseCase::new(voice.clone(), FakeResolver, stations(), max_queue),
            voice,
        )
    }

    fn in_room(channel: VoiceChannelId) -> PlayContext {
        PlayContext {
            guild: GUILD,
            user: ALICE,
            user_channel: Some(channel),
        }
    }

    fn not_in_voice() -> PlayContext {
        PlayContext {
            user_channel: None,
            ..in_room(ROOM)
        }
    }

    fn titles(queue: &[TrackInfo]) -> Vec<&str> {
        queue.iter().map(|t| t.title.as_str()).collect()
    }

    #[tokio::test]
    async fn first_play_joins_and_plays_now() {
        let (music, voice) = music(10);

        let outcome = music
            .play(in_room(ROOM), "https://a.example/one")
            .await
            .unwrap();

        assert!(matches!(outcome, PlayOutcome::NowPlaying(ref t) if t.title == "one"));
        assert_eq!(voice.calls(), ["join 10", "enqueue one"]);
    }

    #[tokio::test]
    async fn later_plays_queue_up_in_order() {
        let (music, _) = music(10);
        music
            .play(in_room(ROOM), "https://a.example/one")
            .await
            .unwrap();

        let outcome = music
            .play(in_room(ROOM), "https://a.example/two")
            .await
            .unwrap();

        assert!(matches!(outcome, PlayOutcome::Queued { position: 1, .. }));
        assert_eq!(titles(&music.queue(GUILD).await), ["one", "two"]);
    }

    #[tokio::test]
    async fn playing_needs_a_voice_channel_and_does_not_resolve_or_join() {
        let (music, voice) = music(10);

        let err = music.play(not_in_voice(), "https://a.example/one").await;

        assert!(matches!(err, Err(MusicError::NotInVoice)));
        assert!(matches!(
            music.play_station(not_in_voice(), "lofi").await,
            Err(MusicError::NotInVoice)
        ));
        assert!(voice.calls().is_empty());
    }

    #[tokio::test]
    async fn unresolvable_query_does_not_join() {
        let (music, voice) = music(10);

        let err = music.play(in_room(ROOM), "some song title").await;

        assert!(matches!(
            err,
            Err(MusicError::Resolve(ResolveError::SearchUnavailable))
        ));
        assert!(voice.calls().is_empty());
    }

    #[tokio::test]
    async fn busy_in_another_channel_refuses_but_idle_moves() {
        let (music, voice) = music(10);
        music
            .play(in_room(ROOM), "https://a.example/one")
            .await
            .unwrap();

        let err = music
            .play(in_room(OTHER_ROOM), "https://a.example/two")
            .await;
        assert!(matches!(err, Err(MusicError::BusyElsewhere(ROOM))));

        music.stop(in_room(ROOM)).await.unwrap();
        music
            .play(in_room(OTHER_ROOM), "https://a.example/two")
            .await
            .unwrap();
        assert!(voice.calls().contains(&"join 11".to_owned()));
    }

    #[tokio::test]
    async fn queue_is_capped() {
        let (music, _) = music(2);
        music
            .play(in_room(ROOM), "https://a.example/one")
            .await
            .unwrap();
        music
            .play(in_room(ROOM), "https://a.example/two")
            .await
            .unwrap();

        let err = music.play(in_room(ROOM), "https://a.example/three").await;

        assert!(matches!(err, Err(MusicError::QueueFull(2))));
    }

    #[tokio::test]
    async fn radio_replaces_the_queue() {
        let (music, voice) = music(10);
        music
            .play(in_room(ROOM), "https://a.example/one")
            .await
            .unwrap();
        music
            .play(in_room(ROOM), "https://a.example/two")
            .await
            .unwrap();

        let outcome = music.play_station(in_room(ROOM), "LOFI").await.unwrap();

        assert!(matches!(outcome, PlayOutcome::NowPlaying(ref t) if t.live && t.title == "Lofi"));
        assert_eq!(titles(&music.queue(GUILD).await), ["Lofi"]);
        assert!(voice.calls().contains(&"stop".to_owned()));
    }

    #[tokio::test]
    async fn playing_after_a_live_stream_replaces_it() {
        let (music, _) = music(10);
        music.play_station(in_room(ROOM), "lofi").await.unwrap();

        let outcome = music
            .play(in_room(ROOM), "https://a.example/one")
            .await
            .unwrap();

        assert!(matches!(outcome, PlayOutcome::NowPlaying(ref t) if t.title == "one"));
        assert_eq!(titles(&music.queue(GUILD).await), ["one"]);
    }

    #[tokio::test]
    async fn unknown_station() {
        let (music, voice) = music(10);

        let err = music.play_station(in_room(ROOM), "polka").await;

        assert!(matches!(err, Err(MusicError::UnknownStation(ref n)) if n == "polka"));
        assert!(voice.calls().is_empty());
    }

    #[tokio::test]
    async fn skip_returns_the_next_track() {
        let (music, _) = music(10);
        music
            .play(in_room(ROOM), "https://a.example/one")
            .await
            .unwrap();
        music
            .play(in_room(ROOM), "https://a.example/two")
            .await
            .unwrap();

        let next = music.skip(in_room(ROOM)).await.unwrap();

        assert_eq!(next.map(|t| t.title), Some("two".to_owned()));
        assert_eq!(music.skip(in_room(ROOM)).await.unwrap(), None);
        assert!(matches!(
            music.skip(in_room(ROOM)).await,
            Err(MusicError::NothingPlaying)
        ));
    }

    #[tokio::test]
    async fn controls_need_the_callers_presence_in_the_bots_channel() {
        let (music, voice) = music(10);
        let elsewhere = in_room(OTHER_ROOM);

        assert!(matches!(
            music.skip(elsewhere).await,
            Err(MusicError::NothingPlaying)
        ));

        music
            .play(in_room(ROOM), "https://a.example/one")
            .await
            .unwrap();
        for result in [
            music.skip(elsewhere).await.map(|_| ()),
            music.stop(elsewhere).await,
            music.leave(elsewhere).await,
            music.stop(not_in_voice()).await,
        ] {
            assert!(matches!(result, Err(MusicError::NotInBotChannel(ROOM))));
        }
        assert_eq!(voice.calls(), ["join 10", "enqueue one"]);
    }

    #[tokio::test]
    async fn stop_clears_and_stays_leave_disconnects() {
        let (music, voice) = music(10);
        music
            .play(in_room(ROOM), "https://a.example/one")
            .await
            .unwrap();

        music.stop(in_room(ROOM)).await.unwrap();
        assert!(music.queue(GUILD).await.is_empty());
        assert!(matches!(
            music.stop(in_room(ROOM)).await,
            Err(MusicError::NothingPlaying)
        ));

        music.leave(in_room(ROOM)).await.unwrap();
        assert_eq!(voice.state.lock().unwrap().get(&GUILD), None);
    }

    #[tokio::test]
    async fn concurrent_first_plays_join_once() {
        let (music, voice) = music(10);
        let music = Arc::new(music);

        let (a, b) = tokio::join!(
            music.play(in_room(ROOM), "https://a.example/one"),
            music.play(in_room(ROOM), "https://a.example/two"),
        );

        a.unwrap();
        b.unwrap();
        let joins = voice
            .calls()
            .iter()
            .filter(|c| c.starts_with("join"))
            .count();
        assert_eq!(joins, 1);
        assert_eq!(music.queue(GUILD).await.len(), 2);
    }
}
