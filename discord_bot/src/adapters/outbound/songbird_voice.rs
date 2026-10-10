//! `VoicePort` on Songbird: one voice connection (`Call`) and built-in track
//! queue per guild. Each queued track carries a `TrackMeta` (what to show,
//! where it comes from, its live now-playing title) as Songbird user data.
//!
//! Sources are opened lazily when a track starts (Songbird `Compose`): a
//! queued link is fetched only on its turn, a library object is presigned
//! only then (a long queue can't outlive the URL), and a live station
//! connects with ICY metadata (`icy.rs`). Library audio is Ogg Opus 48 kHz
//! stereo, which Songbird passes through without re-encoding while it is the
//! only track at full volume.
//!
//! Radio reconnect: a live track that ends or errors without the user
//! stopping it has dropped. The station is started again as a fresh track
//! (new connection and decoder; splicing a reconnected byte stream into a
//! running MP3 decoder risks decode errors) after 1, 2, 4, 8, 16 s, giving
//! up after 5 consecutive failures; a minute of good playback resets that.

use std::collections::{HashMap, HashSet};
use std::num::NonZeroU64;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use songbird::events::{Event, EventContext, EventHandler, TrackEvent};
use songbird::input::{AudioStream, AudioStreamError, Compose, HttpRequest, Input};
use songbird::tracks::{PlayMode, Track as SongbirdTrack};
use songbird::{Call, Songbird};
use symphonia::core::io::MediaSource;
use tokio::sync::Mutex;
use uuid::Uuid;

use super::icy::{LiveStream, NowPlaying};
use crate::application::ports::{ObjectStore, VoiceError, VoicePort};
use crate::domain::music::{GuildId, Track, TrackInfo, TrackSource, VoiceChannelId};

/// Longer than the longest library track (15 min), for reconnects and seeks.
const PRESIGN_TTL: Duration = Duration::from_secs(30 * 60);

/// Playback of library objects: where they live and how to fetch them.
pub struct LibraryAudio {
    pub store: Arc<dyn ObjectStore>,
    /// Plain client: presigned URLs point at our own bucket (localhost in dev),
    /// never at user input, so the public-address guard doesn't apply.
    pub http: reqwest_audio::Client,
}

/// What rides along with each Songbird track.
struct TrackMeta {
    info: TrackInfo,
    source: TrackSource,
    /// Live tracks only.
    now_playing: Option<Arc<NowPlaying>>,
    /// For the `tatarbot::timing` log: queued -> playable.
    enqueued_at: Instant,
}

impl TrackMeta {
    fn kind(&self) -> &'static str {
        match &self.source {
            TrackSource::Http(_) if self.info.live => "radio",
            TrackSource::Http(_) => "link",
            TrackSource::Library { .. } => "library",
        }
    }
}

#[derive(Clone)]
pub struct SongbirdVoice {
    inner: Arc<Inner>,
}

struct Inner {
    manager: Arc<Songbird>,
    http: reqwest_audio::Client,
    library: Option<Arc<LibraryAudio>>,
    reconnects: StdMutex<Reconnects>,
}

impl SongbirdVoice {
    pub fn new(
        manager: Arc<Songbird>,
        http: reqwest_audio::Client,
        library: Option<Arc<LibraryAudio>>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                manager,
                http,
                library,
                reconnects: StdMutex::new(Reconnects::default()),
            }),
        }
    }

    fn call(&self, guild: GuildId) -> Option<Arc<Mutex<Call>>> {
        self.inner.call(guild)
    }

    /// Tracks ended by skip/stop/leave must not be reconnected.
    fn mark_user_ended(&self, handles: impl IntoIterator<Item = Uuid>) {
        let mut reconnects = self.inner.reconnects();
        for uuid in handles {
            reconnects.settle(uuid);
        }
    }
}

impl Inner {
    fn call(&self, guild: GuildId) -> Option<Arc<Mutex<Call>>> {
        self.manager.get(songbird_guild(guild)?)
    }

    fn reconnects(&self) -> std::sync::MutexGuard<'_, Reconnects> {
        self.reconnects.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn songbird_track(&self, track: Track) -> Result<SongbirdTrack, VoiceError> {
        let now_playing = track.info.live.then(|| Arc::new(NowPlaying::default()));
        let input = match &track.source {
            TrackSource::Http(url) if track.info.live => Input::Lazy(Box::new(LiveStream {
                client: self.http.clone(),
                url: url.to_string(),
                now_playing: now_playing.clone().unwrap_or_default(),
            })),
            TrackSource::Http(url) => HttpRequest::new(self.http.clone(), url.to_string()).into(),
            TrackSource::Library { key } => {
                let library = self
                    .library
                    .clone()
                    .ok_or_else(|| VoiceError::Other("library playback not configured".into()))?;
                Input::Lazy(Box::new(PresignedObject {
                    library,
                    key: key.clone(),
                }))
            }
        };
        let meta = TrackMeta {
            info: track.info,
            source: track.source,
            now_playing,
            enqueued_at: Instant::now(),
        };
        Ok(SongbirdTrack::new_with_data(input, Arc::new(meta)))
    }

    /// Starts a dropped station again, unless the bot left or something else
    /// is queued by now.
    async fn restart_station(&self, guild: GuildId, meta: &TrackMeta) {
        let Some(call) = self.call(guild) else {
            return;
        };
        let mut call = call.lock().await;
        if call.current_channel().is_none() || !call.queue().is_empty() {
            return;
        }
        let track = Track {
            info: meta.info.clone(),
            source: meta.source.clone(),
        };
        match self.songbird_track(track) {
            Ok(track) => {
                call.enqueue(track).await;
                tracing::info!(guild = guild.0, title = %meta.info.title, "radio reconnected");
            }
            Err(err) => tracing::warn!(guild = guild.0, %err, "radio reconnect failed"),
        }
    }
}

#[cfg(test)]
impl SongbirdVoice {
    /// A non-live input for `source`, as playback would open it.
    pub fn input_for_test(&self, source: &TrackSource) -> Input {
        let track = Track {
            info: TrackInfo {
                title: "test".into(),
                live: false,
                requested_by: crate::domain::music::UserId(1),
                now_playing: None,
            },
            source: source.clone(),
        };
        self.inner.songbird_track(track).unwrap().input
    }
}

// ---- Reconnect policy (pure, unit-tested)

const RECONNECT_BASE: Duration = Duration::from_secs(1);
const MAX_RECONNECTS: u32 = 5;
/// Playing this long counts as a good connection and resets the backoff.
const STABLE_AFTER: Duration = Duration::from_secs(60);
/// Bound on remembered track ids (they're only needed until the track's
/// end event, but nothing tells us when the last event for a track was).
const MAX_SETTLED: usize = 10_000;

/// Which ended tracks were already handled, and per-guild backoff.
#[derive(Debug, Default)]
struct Reconnects {
    settled: HashSet<Uuid>,
    attempts: HashMap<GuildId, u32>,
}

#[derive(Debug, PartialEq, Eq)]
enum OnLiveEnd {
    /// Ended by the user, or already handled (Songbird may report one track
    /// as both errored and ended).
    Ignore,
    ReconnectAfter(Duration),
    GiveUp,
}

impl Reconnects {
    /// Returns true the first time a track is settled.
    fn settle(&mut self, uuid: Uuid) -> bool {
        if self.settled.len() >= MAX_SETTLED {
            self.settled.clear();
        }
        self.settled.insert(uuid)
    }

    fn on_live_end(&mut self, guild: GuildId, track: Uuid, played: Duration) -> OnLiveEnd {
        if !self.settle(track) {
            return OnLiveEnd::Ignore;
        }
        let attempts = self.attempts.entry(guild).or_default();
        if played >= STABLE_AFTER {
            *attempts = 0;
        }
        *attempts += 1;
        if *attempts > MAX_RECONNECTS {
            *attempts = 0;
            return OnLiveEnd::GiveUp;
        }
        OnLiveEnd::ReconnectAfter(RECONNECT_BASE * 2u32.pow(*attempts - 1))
    }
}

/// Track end/error events for one guild: logs failures, restarts dropped
/// stations.
struct TrackEvents {
    inner: Arc<Inner>,
    guild: GuildId,
}

#[async_trait]
impl EventHandler for TrackEvents {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        let EventContext::Track(tracks) = ctx else {
            return None;
        };
        for (state, handle) in *tracks {
            let meta = handle.data::<TrackMeta>();
            if let PlayMode::Errored(err) = &state.playing {
                tracing::warn!(
                    guild = self.guild.0,
                    title = %meta.info.title,
                    error = %err,
                    "track failed to play"
                );
            }
            if !meta.info.live {
                continue;
            }
            let decision =
                self.inner
                    .reconnects()
                    .on_live_end(self.guild, handle.uuid(), state.play_time);
            match decision {
                OnLiveEnd::Ignore => {}
                OnLiveEnd::GiveUp => tracing::warn!(
                    guild = self.guild.0,
                    title = %meta.info.title,
                    "radio stream keeps dropping; giving up"
                ),
                OnLiveEnd::ReconnectAfter(delay) => {
                    tracing::info!(
                        guild = self.guild.0,
                        title = %meta.info.title,
                        retry_in = ?delay,
                        "radio stream dropped; reconnecting"
                    );
                    let (inner, guild) = (self.inner.clone(), self.guild);
                    tokio::spawn(async move {
                        tokio::time::sleep(delay).await;
                        inner.restart_station(guild, &meta).await;
                    });
                }
            }
        }
        None
    }
}

/// Logs when a track's input is open and parsed: the next 20 ms mixer tick
/// sends its first audio.
struct PlayableTiming {
    guild: GuildId,
}

#[async_trait]
impl EventHandler for PlayableTiming {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        if let EventContext::Track(tracks) = ctx {
            for (_, handle) in *tracks {
                let meta = handle.data::<TrackMeta>();
                tracing::debug!(
                    target: "tatarbot::timing",
                    guild = self.guild.0,
                    kind = meta.kind(),
                    title = %meta.info.title,
                    queued_to_playable_ms = meta.enqueued_at.elapsed().as_millis() as u64,
                    "first audio ready"
                );
            }
        }
        None
    }
}

/// Presigns a library object when Songbird starts the track.
struct PresignedObject {
    library: Arc<LibraryAudio>,
    key: String,
}

#[async_trait]
impl Compose for PresignedObject {
    fn create(&mut self) -> Result<AudioStream<Box<dyn MediaSource>>, AudioStreamError> {
        Err(AudioStreamError::Unsupported)
    }

    async fn create_async(
        &mut self,
    ) -> Result<AudioStream<Box<dyn MediaSource>>, AudioStreamError> {
        let url = self
            .library
            .store
            .presign_get(&self.key, PRESIGN_TTL)
            .await
            .map_err(|e| AudioStreamError::Fail(Box::new(e)))?;
        HttpRequest::new(self.library.http.clone(), url)
            .create_async()
            .await
    }

    fn should_create_async(&self) -> bool {
        true
    }
}

/// Disconnects from every guild; used on shutdown so the bot doesn't linger
/// in voice channels until Discord times it out.
pub async fn leave_all(manager: &Songbird) {
    let guilds: Vec<_> = manager.iter().map(|(guild, _)| guild).collect();
    for guild in guilds {
        if let Err(err) = manager.remove(guild).await {
            tracing::warn!(guild = %guild.0, %err, "failed to leave voice channel");
        }
    }
}

fn songbird_guild(guild: GuildId) -> Option<songbird::id::GuildId> {
    NonZeroU64::new(guild.0).map(Into::into)
}

#[async_trait]
impl VoicePort for SongbirdVoice {
    async fn current_channel(&self, guild: GuildId) -> Option<VoiceChannelId> {
        let call = self.call(guild)?;
        let channel = call.lock().await.current_channel()?;
        Some(VoiceChannelId(channel.0.get()))
    }

    async fn join(&self, guild: GuildId, channel: VoiceChannelId) -> Result<(), VoiceError> {
        let (Some(guild_id), Some(channel_id)) =
            (songbird_guild(guild), NonZeroU64::new(channel.0))
        else {
            return Err(VoiceError::Join("invalid id".into()));
        };
        let first_join = self.inner.manager.get(guild_id).is_none();
        let started = Instant::now();
        let call = self
            .inner
            .manager
            .join(guild_id, songbird::id::ChannelId::from(channel_id))
            .await
            .map_err(|e| VoiceError::Join(e.to_string()))?;
        let mut call = call.lock().await;
        // The bot never listens; deafening tells Discord not to send audio.
        if let Err(err) = call.deafen(true).await {
            tracing::debug!(%err, "could not self-deafen");
        }
        tracing::debug!(
            target: "tatarbot::timing",
            guild = guild.0,
            first_join,
            join_ms = started.elapsed().as_millis() as u64,
            "voice joined"
        );
        if first_join {
            call.add_global_event(Event::Track(TrackEvent::Playable), PlayableTiming { guild });
            for event in [TrackEvent::End, TrackEvent::Error] {
                call.add_global_event(
                    Event::Track(event),
                    TrackEvents {
                        inner: self.inner.clone(),
                        guild,
                    },
                );
            }
        }
        tracing::info!(guild = guild.0, channel = channel.0, "joined voice channel");
        Ok(())
    }

    async fn enqueue(&self, guild: GuildId, track: Track) -> Result<usize, VoiceError> {
        let call = self.call(guild).ok_or(VoiceError::NotConnected)?;
        let track = self.inner.songbird_track(track)?;
        let mut call = call.lock().await;
        call.enqueue(track).await;
        Ok(call.queue().len().saturating_sub(1))
    }

    async fn queue(&self, guild: GuildId) -> Vec<TrackInfo> {
        let Some(call) = self.call(guild) else {
            return Vec::new();
        };
        let handles = call.lock().await.queue().current_queue();
        handles
            .iter()
            .map(|handle| {
                let meta = handle.data::<TrackMeta>();
                TrackInfo {
                    now_playing: meta.now_playing.as_ref().and_then(|n| n.get()),
                    ..meta.info.clone()
                }
            })
            .collect()
    }

    async fn skip(&self, guild: GuildId) -> Result<(), VoiceError> {
        let call = self.call(guild).ok_or(VoiceError::NotConnected)?;
        let call = call.lock().await;
        if let Some(current) = call.queue().current() {
            self.mark_user_ended([current.uuid()]);
        }
        call.queue()
            .skip()
            .map_err(|e| VoiceError::Other(e.to_string()))
    }

    async fn stop(&self, guild: GuildId) -> Result<(), VoiceError> {
        let call = self.call(guild).ok_or(VoiceError::NotConnected)?;
        let call = call.lock().await;
        self.mark_user_ended(call.queue().current_queue().iter().map(|h| h.uuid()));
        call.queue().stop();
        Ok(())
    }

    async fn leave(&self, guild: GuildId) -> Result<(), VoiceError> {
        let guild_id = songbird_guild(guild).ok_or(VoiceError::NotConnected)?;
        if let Some(call) = self.inner.manager.get(guild_id) {
            let call = call.lock().await;
            self.mark_user_ended(call.queue().current_queue().iter().map(|h| h.uuid()));
            call.queue().stop();
        }
        self.inner
            .manager
            .remove(guild_id)
            .await
            .map_err(|e| VoiceError::Other(e.to_string()))?;
        tracing::info!(guild = guild.0, "left voice channel");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use songbird::input::LiveInput;
    use songbird::input::codecs::{get_codec_registry, get_probe};
    use url::Url;

    use super::*;
    use crate::adapters::outbound::audio_http;
    use crate::domain::music::StationList;

    const GUILD: GuildId = GuildId(1);
    const MINUTE: Duration = Duration::from_secs(60);

    #[test]
    fn dropped_streams_back_off_then_give_up() {
        let mut r = Reconnects::default();
        let delays: Vec<_> = (0..6)
            .map(|_| r.on_live_end(GUILD, Uuid::new_v4(), Duration::from_secs(2)))
            .collect();
        assert_eq!(
            delays,
            [1, 2, 4, 8, 16]
                .map(|s| OnLiveEnd::ReconnectAfter(Duration::from_secs(s)))
                .into_iter()
                .chain([OnLiveEnd::GiveUp])
                .collect::<Vec<_>>()
        );
        // After giving up, a new /radio starts from scratch.
        assert_eq!(
            r.on_live_end(GUILD, Uuid::new_v4(), Duration::ZERO),
            OnLiveEnd::ReconnectAfter(Duration::from_secs(1))
        );
    }

    #[test]
    fn a_minute_of_playback_resets_the_backoff() {
        let mut r = Reconnects::default();
        r.on_live_end(GUILD, Uuid::new_v4(), Duration::ZERO);
        r.on_live_end(GUILD, Uuid::new_v4(), Duration::ZERO);
        assert_eq!(
            r.on_live_end(GUILD, Uuid::new_v4(), MINUTE),
            OnLiveEnd::ReconnectAfter(Duration::from_secs(1))
        );
    }

    #[test]
    fn user_ended_and_duplicate_events_are_ignored() {
        let mut r = Reconnects::default();
        let skipped = Uuid::new_v4();
        r.settle(skipped);
        assert_eq!(r.on_live_end(GUILD, skipped, MINUTE), OnLiveEnd::Ignore);

        let dropped = Uuid::new_v4();
        assert!(matches!(
            r.on_live_end(GUILD, dropped, MINUTE),
            OnLiveEnd::ReconnectAfter(_)
        ));
        // Errored, then ended: one reconnect.
        assert_eq!(r.on_live_end(GUILD, dropped, MINUTE), OnLiveEnd::Ignore);
    }

    #[test]
    fn backoff_is_per_guild() {
        let mut r = Reconnects::default();
        for _ in 0..3 {
            r.on_live_end(GUILD, Uuid::new_v4(), Duration::ZERO);
        }
        assert_eq!(
            r.on_live_end(GuildId(2), Uuid::new_v4(), Duration::ZERO),
            OnLiveEnd::ReconnectAfter(Duration::from_secs(1))
        );
    }

    fn voice() -> SongbirdVoice {
        crate::tls::install_crypto_provider().unwrap();
        SongbirdVoice::new(Songbird::serenity(), audio_http::client().unwrap(), None)
    }

    /// Network test: every default station opens and decodes, as a live
    /// track (ICY stream) exactly as the voice adapter plays it.
    /// Run with `cargo test -- --ignored stations_decode`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "needs internet access"]
    async fn stations_decode() {
        let voice = voice();
        let stations = StationList::from_toml(include_str!("../../../stations.toml")).unwrap();
        for station in stations.iter() {
            let track = Track::from_station(station, crate::domain::music::UserId(1));
            let input = voice.inner.songbird_track(track).unwrap().input;
            let input = input
                .make_playable_async(get_codec_registry(), get_probe())
                .await
                .unwrap_or_else(|e| panic!("{}: {e}", station.name));
            let Input::Live(LiveInput::Parsed(mut parsed), _) = input else {
                panic!("{}: not parsed", station.name);
            };
            tokio::task::spawn_blocking(move || {
                for _ in 0..20 {
                    let packet = parsed.format.next_packet().expect("packet");
                    parsed.decoder.decode(&packet).expect("decodable audio");
                }
            })
            .await
            .unwrap();
        }
    }

    /// The SSRF guard also applies inside Songbird's fetch.
    #[tokio::test]
    async fn private_hosts_are_refused_at_play_time() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = Url::parse(&format!(
            "http://localhost:{}/x.mp3",
            listener.local_addr().unwrap().port()
        ))
        .unwrap();

        let result = voice()
            .input_for_test(&TrackSource::Http(url))
            .make_playable_async(get_codec_registry(), get_probe())
            .await;

        assert!(result.is_err(), "fetching localhost must fail");
    }
}
