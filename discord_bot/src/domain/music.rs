//! Music playback types: stations, tracks and the ids they belong to.
//!
//! Ids are plain numbers here; adapters convert from Discord's types. A
//! `Track` carries a `TrackSource`, which says where audio comes from; only
//! the voice adapter interprets it, so a new source (e.g. a server library)
//! adds a variant there without touching the music use case.

use std::collections::HashSet;

use serde::Deserialize;
use url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GuildId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VoiceChannelId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UserId(pub u64);

/// Discord allows at most 25 fixed choices per command option.
pub const MAX_STATIONS: usize = 25;
const MAX_STATION_NAME: usize = 32;
const MAX_TITLE: usize = 100;

/// A curated live stream, addressed by a short `name` (e.g. `lofi`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Station {
    pub name: String,
    pub title: String,
    pub url: Url,
}

/// The configured stations, validated: 1..=25 entries, unique lowercase
/// names (`a-z`, `0-9`, `-`), http(s) URLs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StationList(Vec<Station>);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum StationError {
    #[error("invalid station file: {0}")]
    Parse(String),
    #[error("define between 1 and {MAX_STATIONS} stations")]
    Count,
    #[error("station name {0:?} must be 1-32 characters of a-z, 0-9 or -")]
    Name(String),
    #[error("duplicate station name {0:?}")]
    Duplicate(String),
    #[error("station {0:?}: title must be 1-100 characters")]
    Title(String),
    #[error("station {0:?}: url must be http or https")]
    Url(String),
}

#[derive(Deserialize)]
struct StationFile {
    #[serde(default)]
    station: Vec<StationEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StationEntry {
    name: String,
    title: String,
    url: String,
}

impl StationList {
    /// Parses a TOML file of `[[station]]` tables with `name`, `title`, `url`.
    pub fn from_toml(text: &str) -> Result<Self, StationError> {
        let file: StationFile =
            toml::from_str(text).map_err(|e| StationError::Parse(e.message().to_owned()))?;
        if file.station.is_empty() || file.station.len() > MAX_STATIONS {
            return Err(StationError::Count);
        }
        let mut seen = HashSet::new();
        let mut stations = Vec::with_capacity(file.station.len());
        for entry in file.station {
            let name = entry.name.trim().to_owned();
            let valid_name = !name.is_empty()
                && name.len() <= MAX_STATION_NAME
                && name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
            if !valid_name {
                return Err(StationError::Name(name));
            }
            if !seen.insert(name.clone()) {
                return Err(StationError::Duplicate(name));
            }
            let title = entry.title.trim().to_owned();
            if title.is_empty() || title.chars().count() > MAX_TITLE {
                return Err(StationError::Title(name));
            }
            let url = Url::parse(entry.url.trim())
                .ok()
                .filter(|u| matches!(u.scheme(), "http" | "https"))
                .ok_or_else(|| StationError::Url(name.clone()))?;
            stations.push(Station { name, title, url });
        }
        Ok(Self(stations))
    }

    /// Case-insensitive lookup by name.
    pub fn get(&self, name: &str) -> Option<&Station> {
        let name = name.trim();
        self.0.iter().find(|s| s.name.eq_ignore_ascii_case(name))
    }

    pub fn iter(&self) -> impl Iterator<Item = &Station> {
        self.0.iter()
    }
}

/// Where a track's audio comes from. Interpreted only by the voice adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackSource {
    /// An HTTP(S) audio file or live stream, fetched when the track starts.
    Http(Url),
}

/// What is shown about a track, without how to fetch it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackInfo {
    pub title: String,
    /// Live streams never end on their own.
    pub live: bool,
    pub requested_by: UserId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    pub info: TrackInfo,
    pub source: TrackSource,
}

impl Track {
    pub fn from_station(station: &Station, requested_by: UserId) -> Self {
        Self {
            info: TrackInfo {
                title: station.title.clone(),
                live: true,
                requested_by,
            },
            source: TrackSource::Http(station.url.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
        [[station]]
        name = "lofi"
        title = "Lo-fi beats"
        url = "https://radio.example/lofi.mp3"

        [[station]]
        name = "jazz"
        title = "Jazz"
        url = "http://radio.example/jazz"
    "#;

    fn one(name: &str, title: &str, url: &str) -> String {
        format!("[[station]]\nname = {name:?}\ntitle = {title:?}\nurl = {url:?}\n")
    }

    #[test]
    fn parses_and_looks_up_case_insensitively() {
        let list = StationList::from_toml(VALID).unwrap();
        assert_eq!(list.iter().count(), 2);
        assert_eq!(list.get(" LoFi ").unwrap().title, "Lo-fi beats");
        assert!(list.get("rock").is_none());
    }

    #[test]
    fn the_default_station_file_is_valid() {
        let list = StationList::from_toml(include_str!("../../stations.toml")).unwrap();
        assert!(list.get("lofi").is_some());
    }

    #[test]
    fn rejects_invalid_station_files() {
        let cases = [
            ("".to_owned(), StationError::Count),
            (
                one("Lo Fi", "t", "https://a.example"),
                StationError::Name("Lo Fi".into()),
            ),
            (
                one("a", "", "https://a.example"),
                StationError::Title("a".into()),
            ),
            (
                one("a", "t", "ftp://a.example/x"),
                StationError::Url("a".into()),
            ),
            (one("a", "t", "not a url"), StationError::Url("a".into())),
            (
                one("a", "t", "https://a.example") + &one("a", "u", "https://b.example"),
                StationError::Duplicate("a".into()),
            ),
            (
                (0..26)
                    .map(|i| one(&format!("s{i}"), "t", "https://a.example"))
                    .collect(),
                StationError::Count,
            ),
        ];
        for (text, expected) in cases {
            assert_eq!(StationList::from_toml(&text), Err(expected), "{text}");
        }
        assert!(matches!(
            StationList::from_toml("[[station]]\nname = \"a\"\n"),
            Err(StationError::Parse(_))
        ));
    }

    #[test]
    fn station_tracks_are_live() {
        let list = StationList::from_toml(VALID).unwrap();
        let track = Track::from_station(list.get("jazz").unwrap(), UserId(7));
        assert!(track.info.live);
        assert_eq!(track.info.requested_by, UserId(7));
        assert_eq!(
            track.source,
            TrackSource::Http(Url::parse("http://radio.example/jazz").unwrap())
        );
    }
}
