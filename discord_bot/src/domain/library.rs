//! Per-server audio library: uploaded tracks, their limits and identity.
//!
//! A track's audio is identified by the SHA-256 of the *uploaded original*
//! (`ContentHash`), not of the transcoded file: transcoder output can differ
//! between ffmpeg versions, so hashing it would defeat deduplication.

use std::fmt;
use std::time::Duration;

use chrono::{DateTime, Utc};

use super::music::{GuildId, UserId};

/// Upload limits. Discord's own attachment limit is lower on unboosted servers.
pub const MAX_UPLOAD_BYTES: u64 = 25 * 1024 * 1024;
pub const MAX_DURATION: Duration = Duration::from_secs(15 * 60);
pub const MAX_TITLE_CHARS: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ContentHash(String);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("not a SHA-256 hex digest")]
pub struct InvalidHash;

impl ContentHash {
    pub fn from_digest(digest: &[u8]) -> Self {
        Self(digest.iter().map(|b| format!("{b:02x}")).collect())
    }

    pub fn parse(hex: &str) -> Result<Self, InvalidHash> {
        let valid = hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        valid.then(|| Self(hex.to_owned())).ok_or(InvalidHash)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// 64-bit key for `pg_advisory_xact_lock`: the first 8 bytes of the hash.
    /// Collisions only serialise unrelated uploads; they never break safety.
    pub fn lock_key(&self) -> i64 {
        // Validated hex, so this parses; reinterpret the u64 bits as i64.
        u64::from_str_radix(&self.0[..16], 16).unwrap_or_default() as i64
    }

    /// Object key under `prefix` (e.g. `library/<hash>.ogg`).
    pub fn object_key(&self, prefix: &str) -> String {
        format!("{prefix}{}.ogg", self.0)
    }

    /// Inverse of `object_key`; `None` for keys this library didn't write.
    pub fn from_object_key(prefix: &str, key: &str) -> Option<Self> {
        let hex = key.strip_prefix(prefix)?.strip_suffix(".ogg")?;
        Self::parse(hex).ok()
    }
}

impl fmt::Display for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LibraryTrackId(pub i64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryTrack {
    pub id: LibraryTrackId,
    pub guild: GuildId,
    pub title: String,
    pub hash: ContentHash,
    pub duration: Duration,
    pub size_bytes: u64,
    pub uploaded_by: UserId,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("title must be 1-100 characters")]
pub struct InvalidTitle;

/// Trims, drops control characters and collapses whitespace; 1-100 chars.
pub fn normalize_title(raw: &str) -> Result<String, InvalidTitle> {
    let title = raw
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if title.is_empty() || title.chars().count() > MAX_TITLE_CHARS {
        return Err(InvalidTitle);
    }
    Ok(title)
}

/// A default title from an attachment's file name: the stem, with `_`/`-`
/// runs read as spaces ("my_song-final.mp3" -> "my song final"). Truncated.
pub fn title_from_filename(filename: &str) -> String {
    let stem = filename.rsplit_once('.').map_or(filename, |(stem, _)| stem);
    let spaced: String = stem
        .chars()
        .map(|c| if c == '_' || c == '-' { ' ' } else { c })
        .collect();
    let title = normalize_title(&spaced)
        .or_else(|_| normalize_title(&spaced.chars().take(MAX_TITLE_CHARS).collect::<String>()))
        .unwrap_or_else(|_| "Untitled".to_owned());
    title.chars().take(MAX_TITLE_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "ffffffffffffffff0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn hashes_round_trip_through_object_keys() {
        let hash = ContentHash::parse(HASH).unwrap();
        let key = hash.object_key("library/");
        assert_eq!(key, format!("library/{HASH}.ogg"));
        assert_eq!(ContentHash::from_object_key("library/", &key), Some(hash));
        assert_eq!(
            ContentHash::from_object_key("library/", "library/x.ogg"),
            None
        );
        assert_eq!(ContentHash::from_object_key("other/", &key), None);
    }

    #[test]
    fn hashes_are_validated_and_formatted() {
        assert_eq!(ContentHash::parse(&HASH.to_uppercase()), Err(InvalidHash));
        assert_eq!(ContentHash::parse("abc"), Err(InvalidHash));
        assert_eq!(
            ContentHash::from_digest(&[0x00, 0xab, 0x0f]).as_str(),
            "00ab0f"
        );
    }

    #[test]
    fn lock_keys_cover_the_whole_u64_range() {
        assert_eq!(ContentHash::parse(HASH).unwrap().lock_key(), -1);
        let low = ContentHash::parse(&format!("{:0<64}", "0000000000000001")).unwrap();
        assert_eq!(low.lock_key(), 1);
    }

    #[test]
    fn titles_are_normalised() {
        assert_eq!(
            normalize_title("  My\tSong \n Title "),
            Ok("My Song Title".into())
        );
        assert_eq!(normalize_title("   "), Err(InvalidTitle));
        assert_eq!(normalize_title(&"x".repeat(101)), Err(InvalidTitle));
        assert_eq!(normalize_title("เพลง ไทย"), Ok("เพลง ไทย".into()));
    }

    #[test]
    fn default_titles_come_from_file_names() {
        assert_eq!(title_from_filename("my_song-final.mp3"), "my song final");
        assert_eq!(title_from_filename("noext"), "noext");
        assert_eq!(title_from_filename(".mp3"), "Untitled");
        assert_eq!(
            title_from_filename(&format!("{}.ogg", "a".repeat(300))).len(),
            100
        );
    }
}
