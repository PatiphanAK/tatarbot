use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Semaphore;

use crate::application::ports::{
    AddOutcome, AttachmentFetcher, AudioError, AudioProcessor, FetchError, LibraryError,
    LibraryPage, LibraryRepository, LibraryService, NewTrack, ObjectStore, PreparedAudio,
    RepoError, UploadOutcome, UploadRequest,
};
use crate::domain::library::{
    ContentHash, LibraryTrack, LibraryTrackId, MAX_DURATION, MAX_UPLOAD_BYTES, normalize_title,
    title_from_filename,
};
use crate::domain::music::{GuildId, UserId};

pub const PAGE_SIZE: usize = 10;

#[derive(Debug, Clone)]
pub struct LibrarySettings {
    pub prefix: String,
    pub quota_bytes: u64,
    pub max_upload_bytes: u64,
    pub max_duration: Duration,
}

impl LibrarySettings {
    pub fn new(prefix: String, quota_bytes: u64) -> Self {
        Self {
            prefix,
            quota_bytes,
            max_upload_bytes: MAX_UPLOAD_BYTES,
            max_duration: MAX_DURATION,
        }
    }
}

/// Upload pipeline: permission and size checks, download + hash, dedupe,
/// validate + transcode once (one at a time: the task is small and also
/// encodes voice), then `LibraryRepository::add`, which stores the object
/// and the row consistently.
pub struct LibraryUseCase<R, S, A, F> {
    repo: R,
    store: S,
    audio: A,
    fetcher: F,
    settings: LibrarySettings,
    transcode_slots: Semaphore,
}

impl<R, S, A, F> LibraryUseCase<R, S, A, F>
where
    R: LibraryRepository,
    S: ObjectStore,
    A: AudioProcessor,
    F: AttachmentFetcher,
{
    pub fn new(repo: R, store: S, audio: A, fetcher: F, settings: LibrarySettings) -> Self {
        Self {
            repo,
            store,
            audio,
            fetcher,
            settings,
            transcode_slots: Semaphore::new(1),
        }
    }

    fn available(&self) -> Result<(), LibraryError> {
        if self.repo.is_available() {
            Ok(())
        } else {
            Err(LibraryError::Unavailable)
        }
    }

    /// Validates `input` and transcodes it to `output`; returns what was stored.
    async fn prepare(&self, input: &Path, output: &Path) -> Result<(u64, Duration), LibraryError> {
        let _slot = self
            .transcode_slots
            .acquire()
            .await
            .map_err(|_| LibraryError::Internal("transcoder closed".into()))?;
        let probe = self.audio.probe(input).await.map_err(audio_error)?;
        if probe.audio_streams == 0 || probe.duration.is_zero() {
            return Err(LibraryError::NotAudio);
        }
        if probe.duration > self.settings.max_duration {
            return Err(LibraryError::TooLong {
                max: self.settings.max_duration,
            });
        }
        self.audio
            .transcode(input, output)
            .await
            .map_err(audio_error)?;
        // Measure what we actually store, not what the original claimed.
        let stored = self.audio.probe(output).await.map_err(audio_error)?;
        let size = tokio::fs::metadata(output)
            .await
            .map_err(|e| LibraryError::Processing(e.to_string()))?
            .len();
        if stored.audio_streams == 0 || stored.duration.is_zero() || size == 0 {
            return Err(LibraryError::Processing("empty transcode output".into()));
        }
        Ok((size, stored.duration))
    }
}

#[async_trait]
impl<R, S, A, F> LibraryService for LibraryUseCase<R, S, A, F>
where
    R: LibraryRepository,
    S: ObjectStore,
    A: AudioProcessor,
    F: AttachmentFetcher,
{
    async fn upload(&self, request: UploadRequest) -> Result<UploadOutcome, LibraryError> {
        if !request.can_upload {
            return Err(LibraryError::NotAllowed);
        }
        let max_bytes = self.settings.max_upload_bytes;
        if request.attachment.declared_size > max_bytes {
            return Err(LibraryError::TooLarge { max_bytes });
        }
        let title = match &request.title {
            Some(title) => normalize_title(title).map_err(|_| LibraryError::InvalidTitle)?,
            None => title_from_filename(&request.attachment.filename),
        };
        self.available()?;

        let dir = tempfile::tempdir().map_err(|e| LibraryError::Internal(e.to_string()))?;
        let input = dir.path().join("input");
        let output = dir.path().join("output.ogg");
        let fetched = self
            .fetcher
            .fetch(&request.attachment.url, max_bytes, &input)
            .await
            .map_err(|e| match e {
                FetchError::TooLarge => LibraryError::TooLarge { max_bytes },
                FetchError::NotAllowed => LibraryError::NotAllowed,
                FetchError::Failed(reason) => LibraryError::Processing(reason),
            })?;

        if let Some(existing) = self
            .repo
            .find_in_guild(request.guild, &fetched.hash)
            .await
            .map_err(repo_error)?
        {
            return Err(LibraryError::AlreadyInLibrary(existing));
        }

        // Skip transcoding if this exact file is already stored (any server).
        let mut prepared = None;
        if self
            .repo
            .stored_audio(&fetched.hash)
            .await
            .map_err(repo_error)?
            .is_none()
        {
            prepared = Some(self.prepare(&input, &output).await?);
        }

        // At most two rounds: the stored copy can vanish (concurrent delete)
        // between the hint above and `add`, which then asks for the audio.
        for _ in 0..2 {
            let audio = prepared.map(|(size_bytes, duration)| PreparedAudio {
                path: &output,
                size_bytes,
                duration,
            });
            let outcome = self
                .repo
                .add(
                    NewTrack {
                        guild: request.guild,
                        hash: fetched.hash.clone(),
                        title: title.clone(),
                        uploaded_by: request.user,
                        quota_bytes: self.settings.quota_bytes,
                        audio,
                    },
                    &self.store,
                    &self.settings.prefix,
                )
                .await
                .map_err(repo_error)?;
            match outcome {
                AddOutcome::Added {
                    track,
                    reused_audio,
                } => {
                    tracing::info!(
                        guild = request.guild.0,
                        track = track.id.0,
                        bytes = track.size_bytes,
                        reused_audio,
                        "library upload"
                    );
                    return Ok(UploadOutcome {
                        track,
                        reused_audio,
                    });
                }
                AddOutcome::AlreadyInLibrary(track) => {
                    return Err(LibraryError::AlreadyInLibrary(track));
                }
                AddOutcome::QuotaExceeded { used_bytes } => {
                    return Err(LibraryError::QuotaExceeded {
                        used_bytes,
                        quota_bytes: self.settings.quota_bytes,
                    });
                }
                AddOutcome::NeedsAudio if prepared.is_none() => {
                    prepared = Some(self.prepare(&input, &output).await?);
                }
                AddOutcome::NeedsAudio => break,
            }
        }
        Err(LibraryError::Internal(
            "repository asked for audio it was given".into(),
        ))
    }

    async fn list(&self, guild: GuildId, page: usize) -> Result<LibraryPage, LibraryError> {
        self.available()?;
        let page = page.max(1);
        let (tracks, total) = self
            .repo
            .list(guild, (page - 1) * PAGE_SIZE, PAGE_SIZE)
            .await
            .map_err(repo_error)?;
        let used_bytes = self.repo.used_bytes(guild).await.map_err(repo_error)?;
        Ok(LibraryPage {
            tracks,
            page,
            pages: total.div_ceil(PAGE_SIZE).max(1),
            total,
            used_bytes,
            quota_bytes: self.settings.quota_bytes,
        })
    }

    async fn delete(
        &self,
        guild: GuildId,
        user: UserId,
        can_manage: bool,
        id: LibraryTrackId,
    ) -> Result<LibraryTrack, LibraryError> {
        self.available()?;
        let track = self
            .repo
            .get(guild, id)
            .await
            .map_err(repo_error)?
            .ok_or(LibraryError::NotFound)?;
        if track.uploaded_by != user && !can_manage {
            return Err(LibraryError::NotAllowed);
        }
        self.repo
            .remove(guild, id, &self.store, &self.settings.prefix)
            .await
            .map_err(repo_error)?
            .ok_or(LibraryError::NotFound)
    }

    async fn search(
        &self,
        guild: GuildId,
        query: &str,
        limit: usize,
    ) -> Result<Vec<LibraryTrack>, LibraryError> {
        self.available()?;
        self.repo
            .search(guild, query.trim(), limit)
            .await
            .map_err(repo_error)
    }

    fn object_key(&self, hash: &ContentHash) -> String {
        hash.object_key(&self.settings.prefix)
    }
}

fn repo_error(err: RepoError) -> LibraryError {
    match err {
        RepoError::Unavailable => LibraryError::Unavailable,
        other => LibraryError::Internal(other.to_string()),
    }
}

fn audio_error(err: AudioError) -> LibraryError {
    match err {
        AudioError::Unsupported => LibraryError::NotAudio,
        AudioError::Timeout => LibraryError::Processing("timed out".into()),
        AudioError::Failed(reason) => LibraryError::Processing(reason),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use chrono::{DateTime, Utc};

    use super::*;
    use crate::application::ports::{
        Attachment, Fetched, ProbeResult, StoreError, StoredAudio, SweepReport,
    };

    const GUILD: GuildId = GuildId(1);
    const ALICE: UserId = UserId(10);
    const BOB: UserId = UserId(11);

    fn hash(n: u8) -> ContentHash {
        ContentHash::from_digest(&[n; 32])
    }

    /// In-memory repository with the same contract as Postgres (no races here;
    /// those are tested against real Postgres in `pg_library`).
    #[derive(Default)]
    struct FakeRepo {
        rows: Mutex<Vec<LibraryTrack>>,
        down: AtomicBool,
        /// Simulates a concurrent delete: the hint says "stored", `add` disagrees.
        lie_about_stored: AtomicBool,
        next_id: AtomicUsize,
    }

    #[async_trait]
    impl LibraryRepository for std::sync::Arc<FakeRepo> {
        fn is_available(&self) -> bool {
            !self.down.load(Ordering::SeqCst)
        }
        async fn stored_audio(&self, h: &ContentHash) -> Result<Option<StoredAudio>, RepoError> {
            if self.lie_about_stored.load(Ordering::SeqCst) {
                return Ok(Some(StoredAudio {
                    size_bytes: 1,
                    duration: Duration::from_secs(1),
                }));
            }
            let rows = self.rows.lock().unwrap();
            Ok(rows.iter().find(|r| &r.hash == h).map(|r| StoredAudio {
                size_bytes: r.size_bytes,
                duration: r.duration,
            }))
        }
        async fn find_in_guild(
            &self,
            guild: GuildId,
            h: &ContentHash,
        ) -> Result<Option<LibraryTrack>, RepoError> {
            let rows = self.rows.lock().unwrap();
            Ok(rows
                .iter()
                .find(|r| r.guild == guild && &r.hash == h)
                .cloned())
        }
        async fn add(
            &self,
            t: NewTrack<'_>,
            store: &dyn ObjectStore,
            prefix: &str,
        ) -> Result<AddOutcome, RepoError> {
            let existing = {
                let rows = self.rows.lock().unwrap();
                if let Some(r) = rows.iter().find(|r| r.guild == t.guild && r.hash == t.hash) {
                    return Ok(AddOutcome::AlreadyInLibrary(r.clone()));
                }
                rows.iter().find(|r| r.hash == t.hash).cloned()
            };
            let (size_bytes, duration, reused_audio) = match (existing, &t.audio) {
                (Some(r), _) => (r.size_bytes, r.duration, true),
                (None, Some(a)) => {
                    store.put_file(&t.hash.object_key(prefix), a.path).await?;
                    (a.size_bytes, a.duration, false)
                }
                (None, None) => return Ok(AddOutcome::NeedsAudio),
            };
            let mut rows = self.rows.lock().unwrap();
            let used: u64 = rows
                .iter()
                .filter(|r| r.guild == t.guild)
                .map(|r| r.size_bytes)
                .sum();
            if used + size_bytes > t.quota_bytes {
                return Ok(AddOutcome::QuotaExceeded { used_bytes: used });
            }
            let track = LibraryTrack {
                id: LibraryTrackId(self.next_id.fetch_add(1, Ordering::SeqCst) as i64 + 1),
                guild: t.guild,
                title: t.title,
                hash: t.hash,
                duration,
                size_bytes,
                uploaded_by: t.uploaded_by,
                created_at: DateTime::<Utc>::UNIX_EPOCH,
            };
            rows.push(track.clone());
            Ok(AddOutcome::Added {
                track,
                reused_audio,
            })
        }
        async fn remove(
            &self,
            guild: GuildId,
            id: LibraryTrackId,
            _: &dyn ObjectStore,
            _: &str,
        ) -> Result<Option<LibraryTrack>, RepoError> {
            let mut rows = self.rows.lock().unwrap();
            let pos = rows.iter().position(|r| r.guild == guild && r.id == id);
            Ok(pos.map(|i| rows.remove(i)))
        }
        async fn get(
            &self,
            guild: GuildId,
            id: LibraryTrackId,
        ) -> Result<Option<LibraryTrack>, RepoError> {
            let rows = self.rows.lock().unwrap();
            Ok(rows
                .iter()
                .find(|r| r.guild == guild && r.id == id)
                .cloned())
        }
        async fn search(
            &self,
            guild: GuildId,
            q: &str,
            limit: usize,
        ) -> Result<Vec<LibraryTrack>, RepoError> {
            let rows = self.rows.lock().unwrap();
            Ok(rows
                .iter()
                .filter(|r| r.guild == guild && r.title.contains(q))
                .take(limit)
                .cloned()
                .collect())
        }
        async fn list(
            &self,
            guild: GuildId,
            offset: usize,
            limit: usize,
        ) -> Result<(Vec<LibraryTrack>, usize), RepoError> {
            let rows = self.rows.lock().unwrap();
            let mine: Vec<_> = rows.iter().filter(|r| r.guild == guild).cloned().collect();
            let total = mine.len();
            Ok((mine.into_iter().skip(offset).take(limit).collect(), total))
        }
        async fn used_bytes(&self, guild: GuildId) -> Result<u64, RepoError> {
            let rows = self.rows.lock().unwrap();
            Ok(rows
                .iter()
                .filter(|r| r.guild == guild)
                .map(|r| r.size_bytes)
                .sum())
        }
        async fn sweep(
            &self,
            _: &dyn ObjectStore,
            _: &str,
            _: Duration,
        ) -> Result<SweepReport, RepoError> {
            Ok(SweepReport::default())
        }
    }

    #[derive(Default)]
    struct FakeStore {
        puts: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl ObjectStore for std::sync::Arc<FakeStore> {
        async fn put_file(&self, key: &str, _: &Path) -> Result<(), StoreError> {
            self.puts.lock().unwrap().push(key.to_owned());
            Ok(())
        }
        async fn delete(&self, _: &str) -> Result<(), StoreError> {
            Ok(())
        }
        async fn last_modified(&self, _: &str) -> Result<Option<DateTime<Utc>>, StoreError> {
            Ok(None)
        }
        async fn list(&self, _: &str) -> Result<Vec<(String, DateTime<Utc>)>, StoreError> {
            Ok(Vec::new())
        }
        async fn presign_get(&self, key: &str, _: Duration) -> Result<String, StoreError> {
            Ok(format!("https://s3.test/{key}"))
        }
    }

    /// Probes return `duration`; transcodes write a small file.
    struct FakeAudio {
        duration: Duration,
        audio_streams: usize,
        transcodes: AtomicUsize,
    }

    #[async_trait]
    impl AudioProcessor for std::sync::Arc<FakeAudio> {
        async fn probe(&self, _: &Path) -> Result<ProbeResult, AudioError> {
            Ok(ProbeResult {
                duration: self.duration,
                audio_streams: self.audio_streams,
            })
        }
        async fn transcode(&self, _: &Path, output: &Path) -> Result<(), AudioError> {
            self.transcodes.fetch_add(1, Ordering::SeqCst);
            tokio::fs::write(output, vec![0u8; 1000]).await.unwrap();
            Ok(())
        }
    }

    /// Serves fixed bytes per URL; the hash is derived from the URL.
    struct FakeFetcher {
        sizes: HashMap<String, u64>,
    }

    #[async_trait]
    impl AttachmentFetcher for FakeFetcher {
        async fn fetch(&self, url: &str, max: u64, dest: &Path) -> Result<Fetched, FetchError> {
            let size = *self.sizes.get(url).unwrap_or(&100);
            if size > max {
                return Err(FetchError::TooLarge);
            }
            tokio::fs::write(dest, b"x").await.unwrap();
            let n = url.bytes().last().unwrap_or(0);
            Ok(Fetched {
                size_bytes: size,
                hash: hash(n),
            })
        }
    }

    struct Harness {
        library: LibraryUseCase<
            std::sync::Arc<FakeRepo>,
            std::sync::Arc<FakeStore>,
            std::sync::Arc<FakeAudio>,
            FakeFetcher,
        >,
        repo: std::sync::Arc<FakeRepo>,
        store: std::sync::Arc<FakeStore>,
        audio: std::sync::Arc<FakeAudio>,
    }

    fn harness(duration_secs: u64, quota_bytes: u64) -> Harness {
        let repo = std::sync::Arc::new(FakeRepo::default());
        let store = std::sync::Arc::new(FakeStore::default());
        let audio = std::sync::Arc::new(FakeAudio {
            duration: Duration::from_secs(duration_secs),
            audio_streams: 1,
            transcodes: AtomicUsize::new(0),
        });
        let fetcher = FakeFetcher {
            sizes: HashMap::from([("https://cdn/huge".to_owned(), MAX_UPLOAD_BYTES + 1)]),
        };
        Harness {
            library: LibraryUseCase::new(
                repo.clone(),
                store.clone(),
                audio.clone(),
                fetcher,
                LibrarySettings::new("library/".into(), quota_bytes),
            ),
            repo,
            store,
            audio,
        }
    }

    fn upload(guild: GuildId, user: UserId, url: &str) -> UploadRequest {
        UploadRequest {
            guild,
            user,
            can_upload: true,
            attachment: Attachment {
                url: url.into(),
                filename: "my_song.mp3".into(),
                declared_size: 100,
            },
            title: None,
        }
    }

    #[tokio::test]
    async fn upload_transcodes_stores_and_records() {
        let h = harness(180, 1_000_000);

        let out = h
            .library
            .upload(upload(GUILD, ALICE, "https://cdn/a"))
            .await
            .unwrap();

        assert_eq!(out.track.title, "my song");
        assert_eq!(out.track.size_bytes, 1000);
        assert_eq!(out.track.duration, Duration::from_secs(180));
        assert!(!out.reused_audio);
        assert_eq!(
            *h.store.puts.lock().unwrap(),
            [format!("library/{}.ogg", hash(b'a'))]
        );
    }

    #[tokio::test]
    async fn same_file_in_another_server_reuses_the_audio() {
        let h = harness(180, 1_000_000);
        h.library
            .upload(upload(GUILD, ALICE, "https://cdn/a"))
            .await
            .unwrap();

        let out = h
            .library
            .upload(upload(GuildId(2), BOB, "https://cdn/a"))
            .await
            .unwrap();

        assert!(out.reused_audio);
        assert_eq!(h.audio.transcodes.load(Ordering::SeqCst), 1);
        assert_eq!(h.store.puts.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn same_file_twice_in_one_server_is_refused() {
        let h = harness(180, 1_000_000);
        h.library
            .upload(upload(GUILD, ALICE, "https://cdn/a"))
            .await
            .unwrap();

        let err = h.library.upload(upload(GUILD, BOB, "https://cdn/a")).await;

        assert!(matches!(err, Err(LibraryError::AlreadyInLibrary(_))));
    }

    #[tokio::test]
    async fn stored_copy_vanishing_mid_upload_triggers_a_transcode() {
        let h = harness(180, 1_000_000);
        h.repo.lie_about_stored.store(true, Ordering::SeqCst);

        let out = h
            .library
            .upload(upload(GUILD, ALICE, "https://cdn/a"))
            .await
            .unwrap();

        assert!(!out.reused_audio);
        assert_eq!(h.audio.transcodes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn refusals_happen_before_any_work() {
        let h = harness(180, 1_000_000);
        let mut no_role = upload(GUILD, ALICE, "https://cdn/a");
        no_role.can_upload = false;
        let mut declared_big = upload(GUILD, ALICE, "https://cdn/a");
        declared_big.attachment.declared_size = MAX_UPLOAD_BYTES + 1;
        let mut bad_title = upload(GUILD, ALICE, "https://cdn/a");
        bad_title.title = Some("   ".into());

        assert!(matches!(
            h.library.upload(no_role).await,
            Err(LibraryError::NotAllowed)
        ));
        assert!(matches!(
            h.library.upload(declared_big).await,
            Err(LibraryError::TooLarge { .. })
        ));
        assert!(matches!(
            h.library.upload(bad_title).await,
            Err(LibraryError::InvalidTitle)
        ));
        h.repo.down.store(true, Ordering::SeqCst);
        assert!(matches!(
            h.library
                .upload(upload(GUILD, ALICE, "https://cdn/a"))
                .await,
            Err(LibraryError::Unavailable)
        ));
        assert_eq!(h.audio.transcodes.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn the_download_enforces_the_size_limit_too() {
        let h = harness(180, 1_000_000);
        // Discord said 100 bytes; the body is larger.
        let err = h
            .library
            .upload(upload(GUILD, ALICE, "https://cdn/huge"))
            .await;
        assert!(matches!(err, Err(LibraryError::TooLarge { .. })));
    }

    #[tokio::test]
    async fn long_or_silent_files_are_rejected() {
        let h = harness(15 * 60 + 1, 1_000_000);
        assert!(matches!(
            h.library
                .upload(upload(GUILD, ALICE, "https://cdn/a"))
                .await,
            Err(LibraryError::TooLong { .. })
        ));

        let h = harness(0, 1_000_000);
        assert!(matches!(
            h.library
                .upload(upload(GUILD, ALICE, "https://cdn/a"))
                .await,
            Err(LibraryError::NotAudio)
        ));
    }

    #[tokio::test]
    async fn quota_is_per_server() {
        let h = harness(180, 1500);
        h.library
            .upload(upload(GUILD, ALICE, "https://cdn/a"))
            .await
            .unwrap();

        let err = h
            .library
            .upload(upload(GUILD, ALICE, "https://cdn/b"))
            .await;
        assert!(matches!(
            err,
            Err(LibraryError::QuotaExceeded {
                used_bytes: 1000,
                quota_bytes: 1500
            })
        ));
        h.library
            .upload(upload(GuildId(2), ALICE, "https://cdn/b"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn only_the_uploader_or_a_manager_deletes() {
        let h = harness(180, 1_000_000);
        let track = h
            .library
            .upload(upload(GUILD, ALICE, "https://cdn/a"))
            .await
            .unwrap()
            .track;

        assert!(matches!(
            h.library.delete(GUILD, BOB, false, track.id).await,
            Err(LibraryError::NotAllowed)
        ));
        assert!(matches!(
            h.library.delete(GuildId(2), ALICE, true, track.id).await,
            Err(LibraryError::NotFound)
        ));
        h.library.delete(GUILD, BOB, true, track.id).await.unwrap();
        assert!(matches!(
            h.library.delete(GUILD, ALICE, false, track.id).await,
            Err(LibraryError::NotFound)
        ));
    }

    #[tokio::test]
    async fn list_pages_and_reports_usage() {
        let h = harness(180, 1_000_000);
        for n in 0..12u8 {
            let url = format!("https://cdn/{}", (b'a' + n) as char);
            h.library.upload(upload(GUILD, ALICE, &url)).await.unwrap();
        }

        let first = h.library.list(GUILD, 1).await.unwrap();
        let second = h.library.list(GUILD, 2).await.unwrap();

        assert_eq!((first.tracks.len(), first.pages, first.total), (10, 2, 12));
        assert_eq!(second.tracks.len(), 2);
        assert_eq!(first.used_bytes, 12_000);
        assert_eq!(h.library.list(GuildId(9), 1).await.unwrap().pages, 1);
    }
}
