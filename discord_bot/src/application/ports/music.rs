use async_trait::async_trait;

use crate::domain::music::{GuildId, StationList, Track, TrackInfo, UserId, VoiceChannelId};

/// Who is asking, from where. Built by inbound adapters (slash commands
/// today, chat actions later) from whatever they know about the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayContext {
    pub guild: GuildId,
    pub user: UserId,
    /// The caller's current voice channel, if any.
    pub user_channel: Option<VoiceChannelId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlayOutcome {
    NowPlaying(TrackInfo),
    /// `position` 1 is next after the current track.
    Queued {
        track: TrackInfo,
        position: usize,
    },
}

/// Inbound port: everything about music playback. Validation lives here so
/// every caller gets the same rules and errors.
#[async_trait]
pub trait MusicService: Send + Sync {
    /// Plays a query: a URL directly, anything else through the resolver.
    async fn play(&self, ctx: PlayContext, query: &str) -> Result<PlayOutcome, MusicError>;
    /// Replaces whatever is playing with a configured station.
    async fn play_station(&self, ctx: PlayContext, name: &str) -> Result<PlayOutcome, MusicError>;
    /// Skips the current track; returns the one that plays next, if any.
    async fn skip(&self, ctx: PlayContext) -> Result<Option<TrackInfo>, MusicError>;
    /// Stops playback and clears the queue; stays in the voice channel.
    async fn stop(&self, ctx: PlayContext) -> Result<(), MusicError>;
    /// Stops playback and leaves the voice channel.
    async fn leave(&self, ctx: PlayContext) -> Result<(), MusicError>;
    /// The current track first, then the queue.
    async fn queue(&self, guild: GuildId) -> Vec<TrackInfo>;
    /// The voice channel the bot is in, if any (read-only; for status reports).
    async fn voice_channel(&self, guild: GuildId) -> Option<VoiceChannelId>;
    fn stations(&self) -> &StationList;
}

#[derive(Debug, thiserror::Error)]
pub enum MusicError {
    #[error("the caller is not in a voice channel")]
    NotInVoice,
    #[error("the caller is not in the bot's voice channel")]
    NotInBotChannel(VoiceChannelId),
    #[error("the bot is playing in another voice channel")]
    BusyElsewhere(VoiceChannelId),
    #[error("nothing is playing")]
    NothingPlaying,
    #[error("unknown station {0:?}")]
    UnknownStation(String),
    #[error("the queue is full ({0} tracks)")]
    QueueFull(usize),
    #[error(transparent)]
    Resolve(#[from] ResolveError),
    #[error(transparent)]
    Voice(#[from] VoiceError),
}

/// Outbound port: turns a `/play` query into a track. Phase 1 accepts direct
/// URLs only; a server library plugs in here without changing the use case.
#[async_trait]
pub trait TrackResolver: Send + Sync {
    async fn resolve(
        &self,
        guild: GuildId,
        requested_by: UserId,
        query: &str,
    ) -> Result<Track, ResolveError>;
}

/// Lets the composition root pick a resolver at runtime.
#[async_trait]
impl TrackResolver for std::sync::Arc<dyn TrackResolver> {
    async fn resolve(
        &self,
        guild: GuildId,
        requested_by: UserId,
        query: &str,
    ) -> Result<Track, ResolveError> {
        (**self).resolve(guild, requested_by, query).await
    }
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum ResolveError {
    #[error("not a valid link: {0}")]
    InvalidUrl(String),
    #[error("only http and https links can be played")]
    UnsupportedScheme,
    #[error("links to private or local addresses are not allowed")]
    ForbiddenAddress,
    /// The query is not a URL and there is no library to search (yet).
    #[error("searching by title is not available")]
    SearchUnavailable,
    #[error("the server library is temporarily unavailable")]
    LibraryUnavailable,
    #[error("no track matches {0:?}")]
    NotFound(String),
}

/// Outbound port: one voice connection and track queue per guild.
#[async_trait]
pub trait VoicePort: Send + Sync {
    async fn current_channel(&self, guild: GuildId) -> Option<VoiceChannelId>;
    async fn join(&self, guild: GuildId, channel: VoiceChannelId) -> Result<(), VoiceError>;
    /// Appends a track; returns its index in the queue (0 = playing now).
    async fn enqueue(&self, guild: GuildId, track: Track) -> Result<usize, VoiceError>;
    /// The current track first, then the queue.
    async fn queue(&self, guild: GuildId) -> Vec<TrackInfo>;
    async fn skip(&self, guild: GuildId) -> Result<(), VoiceError>;
    /// Stops playback and empties the queue.
    async fn stop(&self, guild: GuildId) -> Result<(), VoiceError>;
    async fn leave(&self, guild: GuildId) -> Result<(), VoiceError>;
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum VoiceError {
    #[error("could not join the voice channel: {0}")]
    Join(String),
    #[error("not connected to a voice channel")]
    NotConnected,
    #[error("voice error: {0}")]
    Other(String),
}
