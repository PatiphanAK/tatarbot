//! `TrackResolver` for Phase 1: direct http(s) links only. Anything that is
//! not a link is a title search, which needs the server library (Phase 1.5);
//! that resolver will wrap this one and handle the non-link case.

use async_trait::async_trait;
use url::Url;

use super::audio_http::is_public_url;
use crate::application::ports::{ResolveError, TrackResolver};
use crate::domain::music::{GuildId, Track, TrackInfo, TrackSource, UserId};

const MAX_TITLE_CHARS: usize = 100;

pub struct UrlTrackResolver;

/// True if the query should be treated as a link rather than a title search.
pub fn looks_like_url(query: &str) -> bool {
    let query = query.trim();
    query.contains("://") && !query.contains(char::is_whitespace)
}

#[async_trait]
impl TrackResolver for UrlTrackResolver {
    async fn resolve(
        &self,
        _guild: GuildId,
        requested_by: UserId,
        query: &str,
    ) -> Result<Track, ResolveError> {
        let query = query.trim();
        if !looks_like_url(query) {
            return Err(ResolveError::SearchUnavailable);
        }
        let url = Url::parse(query).map_err(|e| ResolveError::InvalidUrl(e.to_string()))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(ResolveError::UnsupportedScheme);
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(ResolveError::InvalidUrl("links with credentials".into()));
        }
        if !is_public_url(&url) {
            return Err(ResolveError::ForbiddenAddress);
        }
        Ok(Track {
            info: TrackInfo {
                title: title_from(&url),
                // Unknown until it plays; a direct link is assumed to end.
                live: false,
                requested_by,
            },
            source: TrackSource::Http(url),
        })
    }
}

/// The last path segment ("song.mp3"), else the host.
fn title_from(url: &Url) -> String {
    let segment = url
        .path_segments()
        .and_then(|mut s| s.rfind(|seg| !seg.is_empty()))
        .map(percent_decode);
    let title = segment.unwrap_or_else(|| url.host_str().unwrap_or("link").to_owned());
    title.chars().take(MAX_TITLE_CHARS).collect()
}

fn percent_decode(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes
            .get(i + 1..i + 3)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (bytes[i], hex) {
            (b'%', Some(byte)) => {
                out.push(byte);
                i += 3;
            }
            (byte, _) => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn resolve(query: &str) -> Result<Track, ResolveError> {
        UrlTrackResolver.resolve(GuildId(1), UserId(2), query).await
    }

    #[tokio::test]
    async fn accepts_public_http_links() {
        let track = resolve("  https://cdn.example/music/My%20Song.mp3?x=1  ")
            .await
            .unwrap();
        assert_eq!(track.info.title, "My Song.mp3");
        assert!(!track.info.live);
        assert_eq!(track.info.requested_by, UserId(2));
        assert_eq!(
            track.source,
            TrackSource::Http(Url::parse("https://cdn.example/music/My%20Song.mp3?x=1").unwrap())
        );
        assert_eq!(
            resolve("http://radio.example/").await.unwrap().info.title,
            "radio.example"
        );
    }

    #[tokio::test]
    async fn rejects_everything_else() {
        let cases = [
            ("never gonna give you up", ResolveError::SearchUnavailable),
            ("lofi", ResolveError::SearchUnavailable),
            ("file:///etc/passwd", ResolveError::UnsupportedScheme),
            ("ftp://a.example/x.mp3", ResolveError::UnsupportedScheme),
            ("http://127.0.0.1:8080/x", ResolveError::ForbiddenAddress),
            (
                "http://169.254.170.2/v2/credentials",
                ResolveError::ForbiddenAddress,
            ),
            ("http://localhost/x.mp3", ResolveError::ForbiddenAddress),
        ];
        for (query, expected) in cases {
            assert_eq!(resolve(query).await.err(), Some(expected), "{query}");
        }
        assert!(matches!(
            resolve("https://user:pw@a.example/x").await,
            Err(ResolveError::InvalidUrl(_))
        ));
        assert!(matches!(
            resolve("https://[bad").await,
            Err(ResolveError::InvalidUrl(_))
        ));
    }

    #[test]
    fn decodes_titles() {
        assert_eq!(percent_decode("a%20b%zz%E0%B8%81"), "a b%zzก");
    }
}
