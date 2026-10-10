//! Ports are the only way in and out of the application core.
//!
//! * Inbound (driving) ports are implemented by use cases and called by
//!   inbound adapters such as Discord.
//! * Outbound (driven) ports are implemented by outbound adapters such as
//!   the HTTP agent client and called by use cases.

mod agent;
mod chat;
mod library;
mod music;

pub use agent::{AgentError, AgentPort};
pub use chat::{ChatError, ChatService};
pub use library::{
    AddOutcome, Attachment, AttachmentFetcher, AudioError, AudioProcessor, FetchError, Fetched,
    LibraryError, LibraryPage, LibraryRepository, LibraryService, NewTrack, ObjectStore,
    PreparedAudio, ProbeResult, RepoError, StoreError, StoredAudio, SweepReport, UploadOutcome,
    UploadRequest,
};
pub use music::{
    MusicError, MusicService, PlayContext, PlayOutcome, ResolveError, TrackResolver, VoiceError,
    VoicePort,
};
