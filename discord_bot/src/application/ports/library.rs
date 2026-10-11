use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::domain::library::{ContentHash, LibraryTrack, LibraryTrackId};
use crate::domain::music::{GuildId, UserId};

// ---- Inbound

/// A Discord attachment to import. `declared_size` comes from Discord and is
/// only used to refuse early; the download enforces the limit itself.
#[derive(Debug, Clone)]
pub struct Attachment {
    pub url: String,
    pub filename: String,
    pub declared_size: u64,
}

#[derive(Debug, Clone)]
pub struct UploadRequest {
    pub guild: GuildId,
    pub user: UserId,
    /// Whether the caller holds the upload role (decided by the adapter).
    pub can_upload: bool,
    pub attachment: Attachment,
    pub title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadOutcome {
    pub track: LibraryTrack,
    /// The audio was already stored (by any server) and was not transcoded again.
    pub reused_audio: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryPage {
    pub tracks: Vec<LibraryTrack>,
    pub page: usize,
    pub pages: usize,
    pub total: usize,
    pub used_bytes: u64,
    pub quota_bytes: u64,
}

/// Inbound port: the server library (`/upload`, `/library`, `/delete`, and
/// title lookup for `/play`).
#[async_trait]
pub trait LibraryService: Send + Sync {
    async fn upload(&self, request: UploadRequest) -> Result<UploadOutcome, LibraryError>;
    /// Pages are 1-based.
    async fn list(&self, guild: GuildId, page: usize) -> Result<LibraryPage, LibraryError>;
    /// The uploader, or someone who `can_manage` (upload role), may delete.
    async fn delete(
        &self,
        guild: GuildId,
        user: UserId,
        can_manage: bool,
        id: LibraryTrackId,
    ) -> Result<LibraryTrack, LibraryError>;
    /// Best matches for a title query, best first.
    async fn search(
        &self,
        guild: GuildId,
        query: &str,
        limit: usize,
    ) -> Result<Vec<LibraryTrack>, LibraryError>;
    /// The most recently added tracks, newest first.
    async fn recent(&self, guild: GuildId, limit: usize)
    -> Result<Vec<LibraryTrack>, LibraryError>;
    /// Object key of a track's audio (for playback).
    fn object_key(&self, hash: &ContentHash) -> String;
}

#[derive(Debug, thiserror::Error)]
pub enum LibraryError {
    #[error("the library is temporarily unavailable")]
    Unavailable,
    #[error("not allowed")]
    NotAllowed,
    #[error("file is larger than {max_bytes} bytes")]
    TooLarge { max_bytes: u64 },
    #[error("audio is longer than {max:?}")]
    TooLong { max: Duration },
    #[error("not a supported audio file")]
    NotAudio,
    #[error("already in this server's library")]
    AlreadyInLibrary(LibraryTrack),
    #[error("storage quota exceeded")]
    QuotaExceeded { used_bytes: u64, quota_bytes: u64 },
    #[error("no such track")]
    NotFound,
    #[error("invalid title")]
    InvalidTitle,
    #[error("processing failed: {0}")]
    Processing(String),
    #[error("internal error: {0}")]
    Internal(String),
}

// ---- Outbound

/// Audio that has been validated and transcoded, ready to store.
#[derive(Debug, Clone)]
pub struct PreparedAudio<'a> {
    pub path: &'a Path,
    pub size_bytes: u64,
    pub duration: Duration,
}

#[derive(Debug, Clone)]
pub struct NewTrack<'a> {
    pub guild: GuildId,
    pub hash: ContentHash,
    pub title: String,
    pub uploaded_by: UserId,
    pub quota_bytes: u64,
    /// `None` when the caller expects the audio to be stored already.
    pub audio: Option<PreparedAudio<'a>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddOutcome {
    Added {
        track: LibraryTrack,
        reused_audio: bool,
    },
    AlreadyInLibrary(LibraryTrack),
    /// No stored audio for this hash (anymore) and none was supplied.
    NeedsAudio,
    QuotaExceeded {
        used_bytes: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoredAudio {
    pub size_bytes: u64,
    pub duration: Duration,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SweepReport {
    pub checked: usize,
    pub deleted: usize,
}

/// Outbound port: library metadata (Postgres). Owns the consistency between
/// rows and stored objects, so the operations that touch both take the store.
///
/// * `add` and `remove` serialise on the content hash (a transaction-level
///   advisory lock), so an upload can never attach a row to an object that a
///   concurrent delete is removing.
/// * `sweep` deletes only objects with no row that are older than `grace`, and
///   re-checks both under the same lock, so it can't remove a file whose row
///   is about to be written.
#[async_trait]
pub trait LibraryRepository: Send + Sync {
    /// False while the database is unreachable; callers fail fast.
    fn is_available(&self) -> bool;
    /// Stored audio for a hash in any server (unlocked hint; `add` re-checks).
    async fn stored_audio(&self, hash: &ContentHash) -> Result<Option<StoredAudio>, RepoError>;
    async fn find_in_guild(
        &self,
        guild: GuildId,
        hash: &ContentHash,
    ) -> Result<Option<LibraryTrack>, RepoError>;
    async fn add(
        &self,
        track: NewTrack<'_>,
        store: &dyn ObjectStore,
        prefix: &str,
    ) -> Result<AddOutcome, RepoError>;
    /// Deletes the row; deletes the object too if no other row uses it.
    async fn remove(
        &self,
        guild: GuildId,
        id: LibraryTrackId,
        store: &dyn ObjectStore,
        prefix: &str,
    ) -> Result<Option<LibraryTrack>, RepoError>;
    async fn get(
        &self,
        guild: GuildId,
        id: LibraryTrackId,
    ) -> Result<Option<LibraryTrack>, RepoError>;
    async fn search(
        &self,
        guild: GuildId,
        query: &str,
        limit: usize,
    ) -> Result<Vec<LibraryTrack>, RepoError>;
    async fn list(
        &self,
        guild: GuildId,
        offset: usize,
        limit: usize,
    ) -> Result<(Vec<LibraryTrack>, usize), RepoError>;
    async fn used_bytes(&self, guild: GuildId) -> Result<u64, RepoError>;
    /// Newest first.
    async fn recent(&self, guild: GuildId, limit: usize) -> Result<Vec<LibraryTrack>, RepoError>;
    async fn sweep(
        &self,
        store: &dyn ObjectStore,
        prefix: &str,
        grace: Duration,
    ) -> Result<SweepReport, RepoError>;
}

#[derive(Debug, thiserror::Error)]
pub enum RepoError {
    #[error("database unavailable")]
    Unavailable,
    #[error("object store: {0}")]
    Store(#[from] StoreError),
    #[error("database: {0}")]
    Other(String),
}

/// Outbound port: S3-compatible object storage.
#[async_trait]
pub trait ObjectStore: Send + Sync {
    async fn put_file(&self, key: &str, path: &Path) -> Result<(), StoreError>;
    async fn delete(&self, key: &str) -> Result<(), StoreError>;
    async fn last_modified(&self, key: &str) -> Result<Option<DateTime<Utc>>, StoreError>;
    /// Every object under `prefix` with its last-modified time.
    async fn list(&self, prefix: &str) -> Result<Vec<(String, DateTime<Utc>)>, StoreError>;
    /// A GET URL valid for `ttl`.
    async fn presign_get(&self, key: &str, ttl: Duration) -> Result<String, StoreError>;
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
#[error("{0}")]
pub struct StoreError(pub String);

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProbeResult {
    pub duration: Duration,
    pub audio_streams: usize,
}

/// Outbound port: validation and transcoding (ffprobe / ffmpeg).
#[async_trait]
pub trait AudioProcessor: Send + Sync {
    async fn probe(&self, input: &Path) -> Result<ProbeResult, AudioError>;
    /// Transcodes the first audio stream to Ogg Opus, 48 kHz stereo.
    async fn transcode(&self, input: &Path, output: &Path) -> Result<(), AudioError>;
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum AudioError {
    #[error("not a supported audio file")]
    Unsupported,
    #[error("timed out")]
    Timeout,
    #[error("{0}")]
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetched {
    pub size_bytes: u64,
    pub hash: ContentHash,
}

/// Outbound port: downloads an attachment to a local file, hashing it.
#[async_trait]
pub trait AttachmentFetcher: Send + Sync {
    async fn fetch(&self, url: &str, max_bytes: u64, dest: &Path) -> Result<Fetched, FetchError>;
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum FetchError {
    #[error("larger than allowed")]
    TooLarge,
    #[error("not a Discord attachment URL")]
    NotAllowed,
    #[error("{0}")]
    Failed(String),
}
