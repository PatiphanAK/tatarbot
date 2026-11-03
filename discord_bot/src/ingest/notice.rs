//! What the outbox row says about an upload (`discord_notifications.outcome`): written by the
//! worker's consumer hooks, rendered and sent by the bot.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain::library::{ContentHash, LibraryTrack, LibraryTrackId};
use crate::domain::music::{GuildId, UserId};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Notice {
    Succeeded {
        track: TrackSnapshot,
        reused_audio: bool,
    },
    Failed {
        code: String,
        /// Code-specific data: `existing` track, `used_bytes`/`quota_bytes`, `max_bytes`, ...
        #[serde(default)]
        data: Value,
    },
    Cancelled,
    /// The bot couldn't hand the upload to the worker in time.
    SubmissionFailed,
}

/// A `LibraryTrack` as stored in a notice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackSnapshot {
    pub id: i64,
    pub guild: u64,
    pub title: String,
    pub hash: String,
    pub duration_ms: u64,
    pub size_bytes: u64,
    pub uploaded_by: u64,
    pub created_at: DateTime<Utc>,
}

impl From<&LibraryTrack> for TrackSnapshot {
    fn from(t: &LibraryTrack) -> Self {
        Self {
            id: t.id.0,
            guild: t.guild.0,
            title: t.title.clone(),
            hash: t.hash.as_str().to_owned(),
            duration_ms: t.duration.as_millis().min(u64::MAX as u128) as u64,
            size_bytes: t.size_bytes,
            uploaded_by: t.uploaded_by.0,
            created_at: t.created_at,
        }
    }
}

impl TrackSnapshot {
    pub fn to_track(&self) -> Option<LibraryTrack> {
        Some(LibraryTrack {
            id: LibraryTrackId(self.id),
            guild: GuildId(self.guild),
            title: self.title.clone(),
            hash: ContentHash::parse(&self.hash).ok()?,
            duration: Duration::from_millis(self.duration_ms),
            size_bytes: self.size_bytes,
            uploaded_by: UserId(self.uploaded_by),
            created_at: self.created_at,
        })
    }
}

/// Discord dedupes channel messages by nonce (≤ 25 chars) when `enforce_nonce` is set.
pub fn nonce_for(job_or_key: &str) -> String {
    let digest = <sha2::Sha256 as sha2::Digest>::digest(job_or_key.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("u{}", &hex[..24])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notices_round_trip_and_nonces_fit_discord() {
        let n = Notice::Failed {
            code: "quota_exceeded".into(),
            data: serde_json::json!({ "used_bytes": 1, "quota_bytes": 2 }),
        };
        let v = serde_json::to_value(&n).unwrap();
        assert_eq!(v["kind"], "failed");
        assert_eq!(serde_json::from_value::<Notice>(v).unwrap(), n);
        let nonce = nonce_for("discord-interaction:123");
        assert_eq!(nonce.len(), 25);
        assert_eq!(nonce, nonce_for("discord-interaction:123"));
        assert_ne!(nonce, nonce_for("discord-interaction:124"));
    }
}
