//! `TrackResolver` for Phase 1.5: links go to `UrlTrackResolver`, anything
//! else is a title search in the current server's library.

use std::sync::Arc;

use async_trait::async_trait;

use super::url_resolver::{UrlTrackResolver, looks_like_url};
use crate::application::ports::{LibraryError, LibraryService, ResolveError, TrackResolver};
use crate::domain::music::{GuildId, Track, UserId};

pub struct LibraryTrackResolver {
    urls: UrlTrackResolver,
    library: Arc<dyn LibraryService>,
}

impl LibraryTrackResolver {
    pub fn new(library: Arc<dyn LibraryService>) -> Self {
        Self {
            urls: UrlTrackResolver,
            library,
        }
    }
}

#[async_trait]
impl TrackResolver for LibraryTrackResolver {
    async fn resolve(
        &self,
        guild: GuildId,
        requested_by: UserId,
        query: &str,
    ) -> Result<Track, ResolveError> {
        if looks_like_url(query) {
            return self.urls.resolve(guild, requested_by, query).await;
        }
        let query = query.trim();
        let best = self
            .library
            .search(guild, query, 1)
            .await
            .map_err(|err| match err {
                LibraryError::Unavailable => ResolveError::LibraryUnavailable,
                other => {
                    tracing::warn!(error = %other, "library search failed");
                    ResolveError::LibraryUnavailable
                }
            })?
            .into_iter()
            .next()
            .ok_or_else(|| ResolveError::NotFound(query.to_owned()))?;
        let key = self.library.object_key(&best.hash);
        Ok(Track::from_library(&best, key, requested_by))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use chrono::{DateTime, Utc};

    use super::*;
    use crate::application::ports::{LibraryPage, UploadOutcome, UploadRequest};
    use crate::domain::library::{ContentHash, LibraryTrack, LibraryTrackId};
    use crate::domain::music::TrackSource;

    struct FakeLibrary {
        down: AtomicBool,
    }

    #[async_trait]
    impl LibraryService for FakeLibrary {
        async fn upload(&self, _: UploadRequest) -> Result<UploadOutcome, LibraryError> {
            unimplemented!()
        }
        async fn list(&self, _: GuildId, _: usize) -> Result<LibraryPage, LibraryError> {
            unimplemented!()
        }
        async fn delete(
            &self,
            _: GuildId,
            _: UserId,
            _: bool,
            _: LibraryTrackId,
        ) -> Result<LibraryTrack, LibraryError> {
            unimplemented!()
        }
        async fn search(
            &self,
            guild: GuildId,
            query: &str,
            _: usize,
        ) -> Result<Vec<LibraryTrack>, LibraryError> {
            if self.down.load(Ordering::SeqCst) {
                return Err(LibraryError::Unavailable);
            }
            Ok((query == "chill song")
                .then(|| LibraryTrack {
                    id: LibraryTrackId(1),
                    guild,
                    title: "Chill Song".into(),
                    hash: ContentHash::from_digest(&[7; 32]),
                    duration: Duration::from_secs(60),
                    size_bytes: 10,
                    uploaded_by: UserId(1),
                    created_at: DateTime::<Utc>::UNIX_EPOCH,
                })
                .into_iter()
                .collect())
        }
        fn object_key(&self, hash: &ContentHash) -> String {
            hash.object_key("library/")
        }
    }

    fn resolver(down: bool) -> LibraryTrackResolver {
        LibraryTrackResolver::new(Arc::new(FakeLibrary {
            down: AtomicBool::new(down),
        }))
    }

    #[tokio::test]
    async fn titles_search_the_library_and_links_still_work() {
        let r = resolver(false);

        let track = r
            .resolve(GuildId(5), UserId(9), "  chill song ")
            .await
            .unwrap();
        assert_eq!(track.info.title, "Chill Song");
        assert_eq!(track.info.requested_by, UserId(9));
        assert_eq!(
            track.source,
            TrackSource::Library {
                key: ContentHash::from_digest(&[7; 32]).object_key("library/")
            }
        );

        let link = r
            .resolve(GuildId(5), UserId(9), "https://a.example/x.mp3")
            .await
            .unwrap();
        assert!(matches!(link.source, TrackSource::Http(_)));
        assert_eq!(
            r.resolve(GuildId(5), UserId(9), "http://127.0.0.1/x")
                .await
                .err(),
            Some(ResolveError::ForbiddenAddress)
        );
    }

    #[tokio::test]
    async fn no_match_and_database_down_are_distinct() {
        assert_eq!(
            resolver(false)
                .resolve(GuildId(5), UserId(9), "nope")
                .await
                .err(),
            Some(ResolveError::NotFound("nope".into()))
        );
        assert_eq!(
            resolver(true)
                .resolve(GuildId(5), UserId(9), "chill song")
                .await
                .err(),
            Some(ResolveError::LibraryUnavailable)
        );
    }
}
