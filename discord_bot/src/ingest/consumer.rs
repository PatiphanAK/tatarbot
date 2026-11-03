//! The music library as an in-process ingest consumer (ADR 0004 §1, §9). Every effect
//! happens in the core's transaction: the track row and its notification commit together
//! with the job's state, or not at all. Dedupe, quota and cross-guild reuse are today's
//! `PgLibrary` logic, unchanged.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ingest_core::{Artifact, Consumer, FetchedDecision, Job, JobError, JobResult, ProcessError};
use serde_json::{Value, json};
use sqlx::PgConnection;

use super::notice::{Notice, TrackSnapshot};
use super::outbox;
use crate::adapters::outbound::pg_library::{AddError, PgLibrary};
use crate::application::ports::{
    AddOutcome, LibraryRepository, NewTrack, ObjectStore, PreparedAudio, RepoError,
};
use crate::domain::library::ContentHash;
use crate::domain::music::{GuildId, UserId};

/// The producer id TatarBot is registered under in `ingest.producers`.
pub const PRODUCER_ID: &str = "tatarbot";
const SWEEP_GRACE: Duration = Duration::from_secs(60 * 60);

pub fn tenant(guild: GuildId) -> String {
    format!("discord:guild:{}", guild.0)
}

pub fn actor(user: UserId) -> String {
    format!("discord:user:{}", user.0)
}

fn parse_id(value: &str, prefix: &str) -> Option<u64> {
    value.strip_prefix(prefix)?.parse().ok()
}

pub struct LibraryConsumer {
    library: PgLibrary,
    /// Confirms objects exist (`put_file`), lists and deletes for the sweep.
    store: Arc<dyn ObjectStore>,
    prefix: String,
    quota_bytes: u64,
}

impl LibraryConsumer {
    pub fn new(
        library: PgLibrary,
        store: Arc<dyn ObjectStore>,
        prefix: String,
        quota_bytes: u64,
    ) -> Self {
        Self {
            library,
            store,
            prefix,
            quota_bytes,
        }
    }

    fn guild(job: &Job) -> Result<GuildId, ProcessError> {
        parse_id(&job.tenant, "discord:guild:")
            .map(GuildId)
            .ok_or_else(|| ProcessError::permanent("policy_denied", "bad tenant".to_owned()))
    }

    fn hash(sha256: &str) -> Result<ContentHash, ProcessError> {
        ContentHash::parse(sha256).map_err(|e| ProcessError::Transient(e.to_string()))
    }
}

fn repo_error(e: RepoError) -> ProcessError {
    ProcessError::Transient(e.to_string())
}

fn db_error(e: sqlx::Error) -> ProcessError {
    ProcessError::Transient(e.to_string())
}

fn already_in_library(track: &crate::domain::library::LibraryTrack) -> ProcessError {
    ProcessError::Permanent {
        code: "already_in_library".into(),
        detail: None,
        data: json!({ "existing": TrackSnapshot::from(track) }),
    }
}

#[async_trait]
impl Consumer for LibraryConsumer {
    async fn on_fetched(&self, job: &Job, sha256: &str) -> Result<FetchedDecision, ProcessError> {
        let guild = Self::guild(job)?;
        let hash = Self::hash(sha256)?;
        if let Some(existing) = self
            .library
            .find_in_guild(guild, &hash)
            .await
            .map_err(repo_error)?
        {
            return Ok(FetchedDecision::Reject(already_in_library(&existing)));
        }
        // Unlocked hint, as in local uploads; `finalize_succeeded` re-checks under the lock.
        Ok(
            match self.library.stored_audio(&hash).await.map_err(repo_error)? {
                Some(stored) => {
                    let key = hash.object_key(&self.prefix);
                    FetchedDecision::Reuse(Artifact {
                        uri: key.clone(),
                        key,
                        sha256: None,
                        size: stored.size_bytes,
                        media_type: "audio/ogg".into(),
                        metadata: json!({ "duration_ms": stored.duration.as_millis() as u64 }),
                    })
                }
                None => FetchedDecision::Continue,
            },
        )
    }

    fn output_key(&self, _job: &Job, sha256: &str, _extension: &str) -> Option<String> {
        ContentHash::parse(sha256)
            .ok()
            .map(|h| h.object_key(&self.prefix))
    }

    async fn finalize_succeeded(
        &self,
        tx: &mut PgConnection,
        job: &Job,
        result: &JobResult,
    ) -> Result<Value, ProcessError> {
        let request = outbox::request(tx, &job.idempotency_key)
            .await
            .map_err(db_error)?
            .ok_or_else(|| ProcessError::permanent("internal", "no upload request".to_owned()))?;
        let hash = Self::hash(&result.source_sha256)?;
        let artifact = result
            .artifacts
            .first()
            .ok_or_else(|| ProcessError::Transient("no artifact".into()))?;
        let duration = Duration::from_millis(
            artifact.metadata["duration_ms"]
                .as_u64()
                .ok_or_else(|| ProcessError::Transient("artifact without duration".into()))?,
        );
        let audio = (!result.reused).then_some(PreparedAudio {
            path: Path::new(""),
            size_bytes: artifact.size,
            duration,
        });
        let outcome = PgLibrary::add_in(
            tx,
            NewTrack {
                guild: GuildId(request.guild_id),
                hash,
                title: request.title,
                uploaded_by: UserId(request.user_id),
                quota_bytes: self.quota_bytes,
                audio,
            },
            self.store.as_ref(),
            &self.prefix,
        )
        .await
        .map_err(|e| match e {
            AddError::Db(e) => db_error(e),
            AddError::Repo(e) => repo_error(e),
        })?;
        match outcome {
            AddOutcome::Added {
                track,
                reused_audio,
            } => {
                let notice = Notice::Succeeded {
                    track: TrackSnapshot::from(&track),
                    reused_audio,
                };
                outbox::insert_notice(tx, &job.idempotency_key, &notice)
                    .await
                    .map_err(db_error)?;
                Ok(json!({ "track_id": track.id.0, "reused_audio": reused_audio }))
            }
            AddOutcome::AlreadyInLibrary(track) => Err(already_in_library(&track)),
            AddOutcome::QuotaExceeded { used_bytes } => Err(ProcessError::Permanent {
                code: "quota_exceeded".into(),
                detail: None,
                data: json!({ "used_bytes": used_bytes, "quota_bytes": self.quota_bytes }),
            }),
            // The reused copy was deleted since `on_fetched`; the retry transcodes.
            AddOutcome::NeedsAudio => Err(ProcessError::Transient("stored audio vanished".into())),
        }
    }

    async fn finalize_failed(
        &self,
        tx: &mut PgConnection,
        job: &Job,
        error: &JobError,
        data: &Value,
    ) -> Result<(), ProcessError> {
        let notice = Notice::Failed {
            code: error.code.clone(),
            data: data.clone(),
        };
        outbox::insert_notice(tx, &job.idempotency_key, &notice)
            .await
            .map_err(db_error)?;
        Ok(())
    }

    async fn finalize_cancelled(
        &self,
        tx: &mut PgConnection,
        job: &Job,
    ) -> Result<(), ProcessError> {
        outbox::insert_notice(tx, &job.idempotency_key, &Notice::Cancelled)
            .await
            .map_err(db_error)?;
        Ok(())
    }

    async fn maintain(&self) -> Result<Value, String> {
        let report = self
            .library
            .sweep(self.store.as_ref(), &self.prefix, SWEEP_GRACE)
            .await
            .map_err(|e| e.to_string())?;
        Ok(json!({ "sweep": { "checked": report.checked, "deleted": report.deleted } }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tenants_and_actors_are_namespaced() {
        assert_eq!(tenant(GuildId(42)), "discord:guild:42");
        assert_eq!(actor(UserId(7)), "discord:user:7");
        assert_eq!(parse_id("discord:guild:42", "discord:guild:"), Some(42));
        assert_eq!(parse_id("uu:org:42", "discord:guild:"), None);
    }
}

/// End to end against real Postgres and ffmpeg: the ingest engine, the `audio.opus-v1`
/// pipeline and this consumer, with a local HTTP source and a directory as the bucket.
/// Run with `make test-db`.
#[cfg(test)]
mod pg_tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use async_trait::async_trait;
    use chrono::{DateTime, Utc};
    use ingest_core::fetch::HttpsFetcher;
    use ingest_core::gcp::LocalArtifacts;
    use ingest_core::{
        ArtifactStore, Delivery, Engine, JobState, ProcessError, Producer, Repo, Source,
        SourceKind, SubmitRequest, TaskQueue,
    };
    use sqlx::PgPool;
    use uuid::Uuid;

    use super::*;
    use crate::adapters::outbound::ffmpeg::{Ffmpeg, FfmpegSettings};
    use crate::application::ports::StoreError;
    use crate::ingest::notice::Notice;
    use crate::ingest::pipeline::{AudioOpusPipeline, PIPELINE_ID};

    const PREFIX: &str = "library/";

    struct NoQueue;

    #[async_trait]
    impl TaskQueue for NoQueue {
        async fn enqueue(&self, _: Uuid, _: i32) -> Result<(), String> {
            Ok(())
        }
    }

    /// Writes nothing: simulates an object that vanished before the commit.
    struct NullArtifacts;

    #[async_trait]
    impl ArtifactStore for NullArtifacts {
        async fn put_if_absent(
            &self,
            key: &str,
            _: &Path,
            _: &str,
        ) -> Result<String, ProcessError> {
            Ok(key.to_owned())
        }
    }

    /// The bucket as a directory: what the worker's `GcsObjectStore` does, locally.
    struct DirStore(PathBuf);

    #[async_trait]
    impl ObjectStore for DirStore {
        async fn put_file(&self, key: &str, _: &Path) -> Result<(), StoreError> {
            if self.0.join(key).exists() {
                Ok(())
            } else {
                Err(StoreError(format!("{key} missing")))
            }
        }
        async fn delete(&self, key: &str) -> Result<(), StoreError> {
            let _ = std::fs::remove_file(self.0.join(key));
            Ok(())
        }
        async fn last_modified(&self, key: &str) -> Result<Option<DateTime<Utc>>, StoreError> {
            Ok(std::fs::metadata(self.0.join(key))
                .ok()
                .and_then(|m| m.modified().ok())
                .map(DateTime::<Utc>::from))
        }
        async fn list(&self, _: &str) -> Result<Vec<(String, DateTime<Utc>)>, StoreError> {
            Ok(Vec::new())
        }
        async fn presign_get(&self, key: &str, _: Duration) -> Result<String, StoreError> {
            Ok(key.to_owned())
        }
    }

    struct H {
        engine: Engine,
        pool: PgPool,
        bucket: tempfile::TempDir,
        base: String,
    }

    async fn tone_bytes(secs: u32) -> Vec<u8> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.mp3");
        let ok = tokio::process::Command::new("ffmpeg")
            .args(["-v", "error", "-f", "lavfi", "-i"])
            .arg(format!("sine=frequency=440:duration={secs}"))
            .arg(&path)
            .status()
            .await
            .unwrap()
            .success();
        assert!(ok);
        std::fs::read(path).unwrap()
    }

    async fn harness(pool: PgPool, quota: u64, artifacts: Option<Arc<dyn ArtifactStore>>) -> H {
        ingest_core::migrate::migrate(&pool).await.unwrap();
        let repo = Repo::new(pool.clone());
        repo.upsert_producer(&Producer {
            id: PRODUCER_ID.into(),
            service_account: "bot@p.iam.gserviceaccount.com".into(),
            tenant_prefix: "discord:".into(),
            allowed_pipelines: vec![PIPELINE_ID.into()],
            allowed_source_kinds: vec!["https_url".into()],
            source_url_hosts: vec!["127.0.0.1".into()],
            max_source_bytes: 26_214_400,
            max_active_per_actor: 3,
            max_jobs_per_tenant_hour: 20,
            enabled: true,
        })
        .await
        .unwrap();
        let bucket = tempfile::tempdir().unwrap();
        let tone = tone_bytes(2).await;
        let router = axum::Router::new().route(
            "/a/song.mp3",
            axum::routing::get(move || async move { tone.clone() }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let consumer = LibraryConsumer::new(
            PgLibrary::from_pool(pool.clone(), true),
            Arc::new(DirStore(bucket.path().to_owned())),
            PREFIX.into(),
            quota,
        );
        let artifacts =
            artifacts.unwrap_or_else(|| Arc::new(LocalArtifacts(bucket.path().to_owned())));
        let engine = Engine::builder(
            repo,
            artifacts,
            Arc::new(NoQueue),
            HttpsFetcher::new(Duration::from_secs(10))
                .unwrap()
                .allowing_loopback_http(),
        )
        .pipeline(
            PIPELINE_ID,
            Arc::new(AudioOpusPipeline::new(Ffmpeg::new(
                FfmpegSettings::default(),
            ))),
        )
        .consumer(PRODUCER_ID, Arc::new(consumer))
        .build();
        H {
            engine,
            pool,
            bucket,
            base,
        }
    }

    impl H {
        /// What the bot does on /upload, then the worker's processing.
        async fn upload(&self, interaction: u64, guild: u64) -> Uuid {
            let key = crate::ingest::producer::idempotency_key(interaction);
            sqlx::query(
                "INSERT INTO library_upload_requests (idempotency_key, application_id, guild_id,
                     channel_id, user_id, title, attachment_url, declared_size, interaction_token,
                     token_expires_at)
                 VALUES ($1, 1, $2, 3, 4, 'Test Tone', $3, 1000, 'tok', now() + interval '15 minutes')",
            )
            .bind(&key)
            .bind(guild as i64)
            .bind(format!("{}/a/song.mp3", self.base))
            .execute(&self.pool)
            .await
            .unwrap();
            let producer = self
                .engine
                .repo()
                .producer(PRODUCER_ID)
                .await
                .unwrap()
                .unwrap();
            let (job, _) = self
                .engine
                .submit(
                    &producer,
                    &SubmitRequest {
                        idempotency_key: key,
                        tenant: tenant(GuildId(guild)),
                        actor: actor(UserId(4)),
                        pipeline: PIPELINE_ID.into(),
                        params: json!({ "max_duration_s": 900 }),
                        source: Source {
                            kind: SourceKind::HttpsUrl,
                            uri: format!("{}/a/song.mp3", self.base),
                            declared_size: Some(1000),
                        },
                    },
                )
                .await
                .unwrap();
            job.id
        }

        async fn notice(&self, interaction: u64) -> Option<Notice> {
            let v: Option<Value> = sqlx::query_scalar(
                "SELECT outcome FROM discord_notifications WHERE request_key = $1",
            )
            .bind(crate::ingest::producer::idempotency_key(interaction))
            .fetch_optional(&self.pool)
            .await
            .unwrap();
            v.map(|v| serde_json::from_value(v).unwrap())
        }

        async fn tracks(&self, guild: u64) -> i64 {
            sqlx::query_scalar("SELECT count(*) FROM library_tracks WHERE guild_id = $1")
                .bind(guild as i64)
                .fetch_one(&self.pool)
                .await
                .unwrap()
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs DATABASE_URL and ffmpeg (make test-db)"]
    async fn ingest_upload_adds_the_track_and_its_notice_atomically(pool: PgPool) {
        let h = harness(pool, 10_000_000, None).await;
        let job = h.upload(1, 100).await;
        assert_eq!(h.engine.process(job).await.unwrap(), Delivery::Done);
        let done = h.engine.repo().job(job).await.unwrap().unwrap();
        assert_eq!(done.state, JobState::Succeeded, "{:?}", done.error_code);
        assert_eq!(h.tracks(100).await, 1);
        let Some(Notice::Succeeded {
            track,
            reused_audio,
        }) = h.notice(1).await
        else {
            panic!("expected a success notice");
        };
        assert!(!reused_audio);
        assert_eq!(track.title, "Test Tone");
        assert!((1_800..=2_200).contains(&track.duration_ms));
        // The object is where playback looks for it.
        let key = ContentHash::parse(&track.hash).unwrap().object_key(PREFIX);
        assert!(h.bucket.path().join(key).exists());
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs DATABASE_URL and ffmpeg (make test-db)"]
    async fn ingest_dedupes_per_guild_and_reuses_across_guilds(pool: PgPool) {
        let h = harness(pool, 10_000_000, None).await;
        let first = h.upload(1, 100).await;
        h.engine.process(first).await.unwrap();
        // Same file, same guild: refused before any transcode.
        let dup = h.upload(2, 100).await;
        h.engine.process(dup).await.unwrap();
        let failed = h.engine.repo().job(dup).await.unwrap().unwrap();
        assert_eq!(failed.error_code.as_deref(), Some("already_in_library"));
        assert!(
            matches!(h.notice(2).await, Some(Notice::Failed { ref code, .. }) if code == "already_in_library")
        );
        assert_eq!(h.tracks(100).await, 1);
        // Same file, another guild: stored audio reused.
        let other = h.upload(3, 200).await;
        h.engine.process(other).await.unwrap();
        assert!(matches!(
            h.notice(3).await,
            Some(Notice::Succeeded {
                reused_audio: true,
                ..
            })
        ));
        assert_eq!(h.tracks(200).await, 1);
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs DATABASE_URL and ffmpeg (make test-db)"]
    async fn ingest_quota_is_enforced_in_the_commit(pool: PgPool) {
        let h = harness(pool, 10, None).await; // 10 bytes per guild
        let job = h.upload(1, 100).await;
        h.engine.process(job).await.unwrap();
        assert_eq!(h.tracks(100).await, 0);
        let Some(Notice::Failed { code, data }) = h.notice(1).await else {
            panic!("expected a failure notice");
        };
        assert_eq!(code, "quota_exceeded");
        assert_eq!(data["quota_bytes"], 10);
    }

    /// The object must exist under the hash lock at commit time; if it doesn't (deleted by a
    /// concurrent sweep, or the upload never landed), nothing is committed and the job retries.
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs DATABASE_URL and ffmpeg (make test-db)"]
    async fn ingest_never_commits_a_track_without_its_object(pool: PgPool) {
        let h = harness(pool, 10_000_000, Some(Arc::new(NullArtifacts))).await;
        let job = h.upload(1, 100).await;
        assert_eq!(h.engine.process(job).await.unwrap(), Delivery::Retry);
        assert_eq!(h.tracks(100).await, 0);
        assert!(h.notice(1).await.is_none());
        assert_eq!(
            h.engine.repo().job(job).await.unwrap().unwrap().state,
            JobState::Queued
        );
    }
}
