//! `LibraryRepository` on Postgres (Supabase in production, via Supavisor in
//! session mode: sqlx uses prepared statements, which transaction mode breaks).
//!
//! The bot must run without the database, so the pool is lazy and a
//! supervisor task owns availability: it connects (and migrates) with
//! backoff, then health-checks. While unavailable every call fails fast with
//! `RepoError::Unavailable`; a connection-level error flips it back.
//!
//! Consistency between rows and S3 objects relies on transaction-level
//! advisory locks on the content hash (`ContentHash::lock_key`), taken by
//! `add`, `remove` and `sweep`, with the object write/delete done while the
//! lock is held. `add` also locks the guild (a separate key space) so two
//! concurrent uploads can't both pass the quota check.

use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{PgPool, Postgres, Transaction};

use crate::application::ports::{
    AddOutcome, LibraryRepository, NewTrack, ObjectStore, RepoError, StoredAudio, SweepReport,
};
use crate::domain::library::{ContentHash, LibraryTrack, LibraryTrackId};
use crate::domain::music::{GuildId, UserId};

const HEALTH_INTERVAL: Duration = Duration::from_secs(60);
/// Supabase pauses free projects without "user database activity" for a week;
/// its docs say a few requests a day are enough. A real table read, 4x a day.
const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const MAX_BACKOFF: Duration = Duration::from_secs(5 * 60);
/// After this long unreachable, log a hint about paused Supabase projects.
const PAUSE_HINT_AFTER: Duration = Duration::from_secs(10 * 60);

/// Minimum pg_trgm score for a fuzzy match (pg_trgm's own default).
const SIMILARITY_THRESHOLD: f32 = 0.3;

/// Key space for guild-level advisory locks (two-int form; never overlaps
/// with the single-bigint hash locks).
const GUILD_LOCK_SPACE: i32 = 1;

#[derive(Clone)]
pub struct PgLibrary {
    pool: PgPool,
    available: Arc<AtomicBool>,
}

struct Row {
    id: i64,
    guild_id: i64,
    content_hash: String,
    title: String,
    duration_ms: i32,
    size_bytes: i64,
    uploaded_by: i64,
    created_at: DateTime<Utc>,
}

impl TryFrom<Row> for LibraryTrack {
    type Error = RepoError;

    fn try_from(row: Row) -> Result<Self, RepoError> {
        Ok(LibraryTrack {
            id: LibraryTrackId(row.id),
            guild: GuildId(row.guild_id as u64),
            hash: ContentHash::parse(&row.content_hash)
                .map_err(|e| RepoError::Other(e.to_string()))?,
            title: row.title,
            duration: Duration::from_millis(row.duration_ms.max(0) as u64),
            size_bytes: row.size_bytes.max(0) as u64,
            uploaded_by: UserId(row.uploaded_by as u64),
            created_at: row.created_at,
        })
    }
}

impl PgLibrary {
    /// Never touches the network: connections open on first use.
    pub fn connect_lazy(url: &str, max_connections: u32) -> Result<Self, sqlx::Error> {
        let options = PgConnectOptions::from_str(url)?;
        let pool = PgPoolOptions::new()
            .max_connections(max_connections)
            .acquire_timeout(Duration::from_secs(5))
            .connect_lazy_with(options);
        Ok(Self::from_pool(pool, false))
    }

    pub fn from_pool(pool: PgPool, available: bool) -> Self {
        Self {
            pool,
            available: Arc::new(AtomicBool::new(available)),
        }
    }

    /// Connects and migrates with backoff, then health-checks and keeps the
    /// project active, forever. Spawn once.
    pub async fn supervise(self) {
        let mut backoff = Duration::from_secs(1);
        let mut down_since = tokio::time::Instant::now();
        let mut hinted = false;
        let mut last_keep_alive = tokio::time::Instant::now();
        loop {
            if !self.available.load(Ordering::SeqCst) {
                match sqlx::migrate!("./migrations").run(&self.pool).await {
                    Ok(()) => {
                        tracing::info!("library database connected");
                        self.log_thai_search_support().await;
                        self.available.store(true, Ordering::SeqCst);
                        backoff = Duration::from_secs(1);
                        hinted = false;
                        continue;
                    }
                    Err(err) => {
                        tracing::warn!(%err, retry_in = ?backoff, "library database unavailable");
                        if !hinted && down_since.elapsed() >= PAUSE_HINT_AFTER {
                            tracing::error!(
                                "library database unreachable for over 10 minutes; if it is a \
                                 Supabase free project it may be paused after a week without \
                                 activity: use \"Resume project\" in the Supabase dashboard"
                            );
                            hinted = true;
                        }
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(MAX_BACKOFF);
                        continue;
                    }
                }
            }

            tokio::time::sleep(HEALTH_INTERVAL).await;
            let keep_alive = last_keep_alive.elapsed() >= KEEP_ALIVE_INTERVAL;
            let check = if keep_alive {
                sqlx::query_scalar!("SELECT count(*) FROM library_tracks")
                    .fetch_one(&self.pool)
                    .await
                    .map(|_| ())
            } else {
                sqlx::query("SELECT 1")
                    .execute(&self.pool)
                    .await
                    .map(|_| ())
            };
            match check {
                Ok(()) if keep_alive => last_keep_alive = tokio::time::Instant::now(),
                Ok(()) => {}
                Err(err) => {
                    tracing::warn!(%err, "library database lost");
                    self.available.store(false, Ordering::SeqCst);
                    down_since = tokio::time::Instant::now();
                }
            }
        }
    }

    /// Whether this database's locale gives Thai trigrams (typo-tolerant Thai
    /// search) or only substring matching. Logged once per connect.
    async fn log_thai_search_support(&self) {
        let fuzzy = sqlx::query_scalar!(r#"SELECT show_trgm('เพลง') <> '{}' AS "fuzzy!""#)
            .fetch_one(&self.pool)
            .await;
        match fuzzy {
            Ok(true) => tracing::info!("title search: fuzzy for Latin and Thai"),
            Ok(false) => tracing::info!(
                "title search: fuzzy for Latin; Thai by substring only (database LC_CTYPE \
                 gives no Thai trigrams)"
            ),
            Err(err) => tracing::warn!(%err, "could not check pg_trgm support"),
        }
    }

    fn ensure_available(&self) -> Result<(), RepoError> {
        if self.available.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(RepoError::Unavailable)
        }
    }

    /// Connection-level failures mark the database unavailable until the
    /// supervisor reconnects; everything else is a plain error.
    fn fail(&self, err: sqlx::Error) -> RepoError {
        let connection_lost = match &err {
            sqlx::Error::Io(_)
            | sqlx::Error::Tls(_)
            | sqlx::Error::PoolTimedOut
            | sqlx::Error::PoolClosed
            | sqlx::Error::WorkerCrashed => true,
            // SQLSTATE 08 (connection exception), 57 (operator intervention).
            sqlx::Error::Database(db) => db
                .code()
                .is_some_and(|code| code.starts_with("08") || code.starts_with("57")),
            _ => false,
        };
        if connection_lost {
            if self.available.swap(false, Ordering::SeqCst) {
                tracing::warn!(%err, "library database lost");
            }
            RepoError::Unavailable
        } else {
            RepoError::Other(err.to_string())
        }
    }

    async fn begin(&self) -> Result<Transaction<'static, Postgres>, RepoError> {
        self.ensure_available()?;
        self.pool.begin().await.map_err(|e| self.fail(e))
    }

    async fn lock_hash(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        hash: &ContentHash,
    ) -> Result<(), RepoError> {
        sqlx::query!("SELECT pg_advisory_xact_lock($1)", hash.lock_key())
            .execute(&mut **tx)
            .await
            .map_err(|e| self.fail(e))?;
        Ok(())
    }
}

/// Postgres `LIKE` pattern escaping (`\` is the escape character).
fn escape_like(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn guild_lock_key(guild: GuildId) -> i32 {
    (guild.0 % i32::MAX as u64) as i32
}

#[async_trait]
impl LibraryRepository for PgLibrary {
    fn is_available(&self) -> bool {
        self.available.load(Ordering::SeqCst)
    }

    async fn stored_audio(&self, hash: &ContentHash) -> Result<Option<StoredAudio>, RepoError> {
        self.ensure_available()?;
        let row = sqlx::query!(
            "SELECT size_bytes, duration_ms FROM library_tracks WHERE content_hash = $1 LIMIT 1",
            hash.as_str()
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| self.fail(e))?;
        Ok(row.map(|r| StoredAudio {
            size_bytes: r.size_bytes as u64,
            duration: Duration::from_millis(r.duration_ms as u64),
        }))
    }

    async fn find_in_guild(
        &self,
        guild: GuildId,
        hash: &ContentHash,
    ) -> Result<Option<LibraryTrack>, RepoError> {
        self.ensure_available()?;
        sqlx::query_as!(
            Row,
            "SELECT * FROM library_tracks WHERE guild_id = $1 AND content_hash = $2",
            guild.0 as i64,
            hash.as_str()
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| self.fail(e))?
        .map(LibraryTrack::try_from)
        .transpose()
    }

    async fn add(
        &self,
        track: NewTrack<'_>,
        store: &dyn ObjectStore,
        prefix: &str,
    ) -> Result<AddOutcome, RepoError> {
        let guild = track.guild.0 as i64;
        let mut tx = self.begin().await?;
        // Guild first, then hash; `remove` and `sweep` take only the hash lock,
        // so the order can't deadlock.
        sqlx::query!(
            "SELECT pg_advisory_xact_lock($1, $2)",
            GUILD_LOCK_SPACE,
            guild_lock_key(track.guild)
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| self.fail(e))?;
        self.lock_hash(&mut tx, &track.hash).await?;

        if let Some(row) = sqlx::query_as!(
            Row,
            "SELECT * FROM library_tracks WHERE guild_id = $1 AND content_hash = $2",
            guild,
            track.hash.as_str()
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| self.fail(e))?
        {
            return Ok(AddOutcome::AlreadyInLibrary(row.try_into()?));
        }

        // Under the lock, rows are the truth: if any exists, its object exists.
        let stored = sqlx::query!(
            "SELECT size_bytes, duration_ms FROM library_tracks WHERE content_hash = $1 LIMIT 1",
            track.hash.as_str()
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| self.fail(e))?;
        let (size_bytes, duration_ms, reused_audio) = match (&stored, &track.audio) {
            (Some(row), _) => (row.size_bytes, row.duration_ms, true),
            (None, Some(audio)) => (
                audio.size_bytes as i64,
                audio.duration.as_millis().min(i32::MAX as u128) as i32,
                false,
            ),
            (None, None) => return Ok(AddOutcome::NeedsAudio),
        };

        let used = sqlx::query_scalar!(
            r#"SELECT COALESCE(SUM(size_bytes), 0)::BIGINT AS "used!" FROM library_tracks WHERE guild_id = $1"#,
            guild
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| self.fail(e))?;
        if (used + size_bytes) as u64 > track.quota_bytes {
            return Ok(AddOutcome::QuotaExceeded {
                used_bytes: used.max(0) as u64,
            });
        }

        if let (false, Some(audio)) = (reused_audio, &track.audio) {
            // While the hash lock is held: no delete or sweep can interleave.
            store
                .put_file(&track.hash.object_key(prefix), audio.path)
                .await?;
        }

        let row = sqlx::query_as!(
            Row,
            "INSERT INTO library_tracks
                 (guild_id, content_hash, title, duration_ms, size_bytes, uploaded_by)
             VALUES ($1, $2, $3, $4, $5, $6)
             RETURNING *",
            guild,
            track.hash.as_str(),
            track.title,
            duration_ms,
            size_bytes,
            track.uploaded_by.0 as i64
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| self.fail(e))?;
        // If this commit fails after a PUT, the object is an orphan: the sweep
        // removes it once it is older than the grace period.
        tx.commit().await.map_err(|e| self.fail(e))?;
        Ok(AddOutcome::Added {
            track: row.try_into()?,
            reused_audio,
        })
    }

    async fn remove(
        &self,
        guild: GuildId,
        id: LibraryTrackId,
        store: &dyn ObjectStore,
        prefix: &str,
    ) -> Result<Option<LibraryTrack>, RepoError> {
        let mut tx = self.begin().await?;
        let Some(hash) = sqlx::query_scalar!(
            "SELECT content_hash FROM library_tracks WHERE guild_id = $1 AND id = $2",
            guild.0 as i64,
            id.0
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| self.fail(e))?
        else {
            return Ok(None);
        };
        let hash = ContentHash::parse(&hash).map_err(|e| RepoError::Other(e.to_string()))?;
        self.lock_hash(&mut tx, &hash).await?;

        // Re-read under the lock: a concurrent delete may have won.
        let Some(row) = sqlx::query_as!(
            Row,
            "DELETE FROM library_tracks WHERE guild_id = $1 AND id = $2 RETURNING *",
            guild.0 as i64,
            id.0
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| self.fail(e))?
        else {
            return Ok(None);
        };
        let remaining = sqlx::query_scalar!(
            r#"SELECT count(*) AS "count!" FROM library_tracks WHERE content_hash = $1"#,
            hash.as_str()
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| self.fail(e))?;
        if remaining == 0 {
            // Still under the lock: an upload of the same hash waits, then sees
            // no row and stores the object again. If the delete fails, the
            // transaction rolls back and the row stays.
            store.delete(&hash.object_key(prefix)).await?;
        }
        tx.commit().await.map_err(|e| self.fail(e))?;
        Ok(Some(row.try_into()?))
    }

    async fn get(
        &self,
        guild: GuildId,
        id: LibraryTrackId,
    ) -> Result<Option<LibraryTrack>, RepoError> {
        self.ensure_available()?;
        sqlx::query_as!(
            Row,
            "SELECT * FROM library_tracks WHERE guild_id = $1 AND id = $2",
            guild.0 as i64,
            id.0
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| self.fail(e))?
        .map(LibraryTrack::try_from)
        .transpose()
    }

    async fn search(
        &self,
        guild: GuildId,
        query: &str,
        limit: usize,
    ) -> Result<Vec<LibraryTrack>, RepoError> {
        self.ensure_available()?;
        let started = std::time::Instant::now();
        let query = query.trim().to_lowercase();
        let escaped = escape_like(&query);
        // Candidates: substring (any locale, so Thai always works) or trigram
        // similarity (typos, word order). Ranked exact > prefix > substring >
        // trigram score, then shorter titles.
        let rows = sqlx::query_as!(
            Row,
            r"SELECT * FROM library_tracks
              WHERE guild_id = $1
                AND (lower(title) LIKE $2 ESCAPE '\'
                     OR word_similarity($3, lower(title)) >= $5
                     OR similarity($3, lower(title)) >= $5)
              ORDER BY CASE WHEN lower(title) = $3 THEN 0
                            WHEN lower(title) LIKE $4 ESCAPE '\' THEN 1
                            WHEN lower(title) LIKE $2 ESCAPE '\' THEN 2
                            ELSE 3 END,
                       greatest(word_similarity($3, lower(title)),
                                similarity($3, lower(title))) DESC,
                       char_length(title),
                       id DESC
              LIMIT $6",
            guild.0 as i64,
            format!("%{escaped}%"),
            query,
            format!("{escaped}%"),
            SIMILARITY_THRESHOLD,
            limit as i64
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| self.fail(e))?;
        tracing::debug!(
            target: "tatarbot::timing",
            guild = guild.0,
            chars = query.chars().count(),
            rows = rows.len(),
            search_ms = started.elapsed().as_secs_f64() * 1000.0,
            "library search"
        );
        rows.into_iter().map(LibraryTrack::try_from).collect()
    }

    async fn list(
        &self,
        guild: GuildId,
        offset: usize,
        limit: usize,
    ) -> Result<(Vec<LibraryTrack>, usize), RepoError> {
        self.ensure_available()?;
        let rows = sqlx::query_as!(
            Row,
            "SELECT * FROM library_tracks WHERE guild_id = $1
             ORDER BY lower(title), id LIMIT $2 OFFSET $3",
            guild.0 as i64,
            limit as i64,
            offset as i64
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| self.fail(e))?;
        let total = sqlx::query_scalar!(
            r#"SELECT count(*) AS "count!" FROM library_tracks WHERE guild_id = $1"#,
            guild.0 as i64
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|e| self.fail(e))?;
        let tracks = rows
            .into_iter()
            .map(LibraryTrack::try_from)
            .collect::<Result<_, _>>()?;
        Ok((tracks, total.max(0) as usize))
    }

    async fn used_bytes(&self, guild: GuildId) -> Result<u64, RepoError> {
        self.ensure_available()?;
        let used = sqlx::query_scalar!(
            r#"SELECT COALESCE(SUM(size_bytes), 0)::BIGINT AS "used!" FROM library_tracks WHERE guild_id = $1"#,
            guild.0 as i64
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|e| self.fail(e))?;
        Ok(used.max(0) as u64)
    }

    async fn sweep(
        &self,
        store: &dyn ObjectStore,
        prefix: &str,
        grace: Duration,
    ) -> Result<SweepReport, RepoError> {
        self.ensure_available()?;
        let grace =
            chrono::Duration::from_std(grace).map_err(|e| RepoError::Other(e.to_string()))?;
        let mut report = SweepReport::default();
        for (key, listed_modified) in store.list(prefix).await? {
            let Some(hash) = ContentHash::from_object_key(prefix, &key) else {
                continue; // not written by the library
            };
            if Utc::now() - listed_modified < grace {
                continue;
            }
            report.checked += 1;
            let mut tx = self.begin().await?;
            self.lock_hash(&mut tx, &hash).await?;
            let rows = sqlx::query_scalar!(
                r#"SELECT count(*) AS "count!" FROM library_tracks WHERE content_hash = $1"#,
                hash.as_str()
            )
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| self.fail(e))?;
            // Re-check the age under the lock: an upload may have re-stored the
            // object (fresh timestamp) since it was listed.
            let still_old = store
                .last_modified(&key)
                .await?
                .is_some_and(|modified| Utc::now() - modified >= grace);
            if rows == 0 && still_old {
                store.delete(&key).await?;
                report.deleted += 1;
                tracing::info!(key, "swept orphaned library object");
            }
            tx.commit().await.map_err(|e| self.fail(e))?;
        }
        Ok(report)
    }
}

/// Runs `sweep` every `interval` while the database is available.
pub async fn sweep_periodically(
    repo: PgLibrary,
    store: Arc<dyn ObjectStore>,
    prefix: String,
    grace: Duration,
    interval: Duration,
) {
    loop {
        tokio::time::sleep(interval).await;
        if !repo.is_available() {
            continue;
        }
        match repo.sweep(store.as_ref(), &prefix, grace).await {
            Ok(report) if report.deleted > 0 => tracing::info!(?report, "library sweep"),
            Ok(_) => {}
            Err(err) => tracing::warn!(%err, "library sweep failed"),
        }
    }
}

/// Against real Postgres: `#[sqlx::test]` creates a fresh, migrated database
/// per test from DATABASE_URL. Run with `make test-db`.
#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::Mutex;

    use tokio::sync::Notify;

    use super::*;
    use crate::application::ports::{PreparedAudio, StoreError};

    const PREFIX: &str = "library/";
    const QUOTA: u64 = 1_000_000;

    type Listing = Vec<(String, DateTime<Utc>)>;

    /// Pauses an operation inside its critical section until released.
    #[derive(Default)]
    struct Gate {
        entered: Notify,
        release: Notify,
    }

    /// In-memory bucket with an operation log and optional gates.
    #[derive(Default)]
    struct FakeStore {
        objects: Mutex<HashMap<String, DateTime<Utc>>>,
        log: Mutex<Vec<String>>,
        put_gate: Mutex<Option<Arc<Gate>>>,
        delete_gate: Mutex<Option<Arc<Gate>>>,
        /// A stale listing, as if taken before concurrent changes.
        listing: Mutex<Option<Listing>>,
    }

    impl FakeStore {
        fn has(&self, key: &str) -> bool {
            self.objects.lock().unwrap().contains_key(key)
        }
        fn insert(&self, key: &str, modified: DateTime<Utc>) {
            self.objects
                .lock()
                .unwrap()
                .insert(key.to_owned(), modified);
        }
        fn log(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }
        fn gate(slot: &Mutex<Option<Arc<Gate>>>) -> Arc<Gate> {
            let gate = Arc::new(Gate::default());
            *slot.lock().unwrap() = Some(gate.clone());
            gate
        }
        async fn pass(slot: &Mutex<Option<Arc<Gate>>>) {
            let gate = slot.lock().unwrap().take();
            if let Some(gate) = gate {
                gate.entered.notify_one();
                gate.release.notified().await;
            }
        }
    }

    #[async_trait]
    impl ObjectStore for FakeStore {
        async fn put_file(&self, key: &str, _: &Path) -> Result<(), StoreError> {
            Self::pass(&self.put_gate).await;
            self.log.lock().unwrap().push(format!("put {key}"));
            self.insert(key, Utc::now());
            Ok(())
        }
        async fn delete(&self, key: &str) -> Result<(), StoreError> {
            Self::pass(&self.delete_gate).await;
            self.log.lock().unwrap().push(format!("delete {key}"));
            self.objects.lock().unwrap().remove(key);
            Ok(())
        }
        async fn last_modified(&self, key: &str) -> Result<Option<DateTime<Utc>>, StoreError> {
            Ok(self.objects.lock().unwrap().get(key).copied())
        }
        async fn list(&self, prefix: &str) -> Result<Vec<(String, DateTime<Utc>)>, StoreError> {
            if let Some(stale) = self.listing.lock().unwrap().take() {
                return Ok(stale);
            }
            let objects = self.objects.lock().unwrap();
            Ok(objects
                .iter()
                .filter(|(k, _)| k.starts_with(prefix))
                .map(|(k, t)| (k.clone(), *t))
                .collect())
        }
        async fn presign_get(&self, key: &str, _: Duration) -> Result<String, StoreError> {
            Ok(key.to_owned())
        }
    }

    fn hash(n: u8) -> ContentHash {
        ContentHash::from_digest(&[n; 32])
    }

    fn audio(size_bytes: u64) -> PreparedAudio<'static> {
        PreparedAudio {
            path: Path::new("/nonexistent/out.ogg"),
            size_bytes,
            duration: Duration::from_secs(120),
        }
    }

    fn new_track(
        guild: u64,
        h: &ContentHash,
        audio: Option<PreparedAudio<'static>>,
    ) -> NewTrack<'static> {
        NewTrack {
            guild: GuildId(guild),
            hash: h.clone(),
            title: format!("track {}", &h.as_str()[..6]),
            uploaded_by: UserId(7),
            quota_bytes: QUOTA,
            audio,
        }
    }

    async fn add(repo: &PgLibrary, store: &FakeStore, t: NewTrack<'static>) -> AddOutcome {
        repo.add(t, store, PREFIX).await.unwrap()
    }

    fn added(outcome: AddOutcome) -> LibraryTrack {
        match outcome {
            AddOutcome::Added { track, .. } => track,
            other => panic!("expected Added, got {other:?}"),
        }
    }

    async fn wait_until_blocked<T>(task: &tokio::task::JoinHandle<T>) {
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !task.is_finished(),
            "operation should be blocked on the advisory lock"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs DATABASE_URL (make test-db)"]
    async fn pg_add_stores_once_and_dedupes(pool: PgPool) {
        let repo = PgLibrary::from_pool(pool, true);
        let store = FakeStore::default();
        let h = hash(1);

        let first = add(&repo, &store, new_track(1, &h, Some(audio(5000)))).await;
        assert!(matches!(
            first,
            AddOutcome::Added {
                reused_audio: false,
                ..
            }
        ));
        // Another server, no audio supplied: reuses the stored object.
        let second = add(&repo, &store, new_track(2, &h, None)).await;
        assert!(
            matches!(second, AddOutcome::Added { reused_audio: true, ref track } if track.size_bytes == 5000)
        );
        // Same server again.
        assert!(matches!(
            add(&repo, &store, new_track(1, &h, Some(audio(5000)))).await,
            AddOutcome::AlreadyInLibrary(_)
        ));
        // Unknown hash without audio.
        assert_eq!(
            add(&repo, &store, new_track(1, &hash(2), None)).await,
            AddOutcome::NeedsAudio
        );
        assert_eq!(store.log(), [format!("put {}", h.object_key(PREFIX))]);
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs DATABASE_URL (make test-db)"]
    async fn pg_remove_deletes_the_object_with_the_last_row(pool: PgPool) {
        let repo = PgLibrary::from_pool(pool, true);
        let store = FakeStore::default();
        let h = hash(1);
        let key = h.object_key(PREFIX);
        let a = added(add(&repo, &store, new_track(1, &h, Some(audio(10)))).await);
        let b = added(add(&repo, &store, new_track(2, &h, None)).await);

        repo.remove(GuildId(1), a.id, &store, PREFIX)
            .await
            .unwrap()
            .unwrap();
        assert!(store.has(&key), "still used by guild 2");
        // Wrong guild: not found, nothing deleted.
        assert_eq!(
            repo.remove(GuildId(1), b.id, &store, PREFIX).await.unwrap(),
            None
        );
        repo.remove(GuildId(2), b.id, &store, PREFIX)
            .await
            .unwrap()
            .unwrap();
        assert!(!store.has(&key));
    }

    /// Race 1: a delete removing the last row (and the object) vs an upload of
    /// the same hash that, from an earlier hint, expects the object to exist.
    /// The upload must wait, then store the object again, never attach a row
    /// to a deleted object.
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs DATABASE_URL (make test-db)"]
    async fn pg_race_delete_vs_upload_of_the_same_hash(pool: PgPool) {
        let repo = PgLibrary::from_pool(pool, true);
        let store = Arc::new(FakeStore::default());
        let h = hash(3);
        let key = h.object_key(PREFIX);
        let existing = added(add(&repo, &store, new_track(1, &h, Some(audio(10)))).await);

        // Delete holds the hash lock while the object deletion is in progress.
        let gate = FakeStore::gate(&store.delete_gate);
        let deleting = tokio::spawn({
            let (repo, store) = (repo.clone(), store.clone());
            async move {
                repo.remove(GuildId(1), existing.id, store.as_ref(), PREFIX)
                    .await
            }
        });
        gate.entered.notified().await;

        // The upload saw "stored" before the delete started, so it brings no audio.
        let uploading = tokio::spawn({
            let (repo, store) = (repo.clone(), store.clone());
            let h = h.clone();
            async move {
                repo.add(new_track(2, &h, None), store.as_ref(), PREFIX)
                    .await
            }
        });
        wait_until_blocked(&uploading).await;

        gate.release.notify_one();
        deleting.await.unwrap().unwrap().unwrap();
        // After the delete committed: no row, no object, so the upload must
        // supply the audio; it does not get a row pointing at nothing.
        assert_eq!(uploading.await.unwrap().unwrap(), AddOutcome::NeedsAudio);
        assert!(!store.has(&key));

        let track = added(add(&repo, &store, new_track(2, &h, Some(audio(10)))).await);
        assert!(store.has(&key), "the upload stored the object again");
        assert_eq!(repo.get(GuildId(2), track.id).await.unwrap(), Some(track));
        assert_eq!(
            store.log(),
            [
                format!("put {key}"),
                format!("delete {key}"),
                format!("put {key}")
            ]
        );
    }

    /// Race 2: the sweep listed an object as an old orphan, but an upload of
    /// that hash is storing it right now. The sweep must wait for the upload's
    /// lock and then keep the object.
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs DATABASE_URL (make test-db)"]
    async fn pg_race_sweep_vs_upload_in_flight(pool: PgPool) {
        let repo = PgLibrary::from_pool(pool, true);
        let store = Arc::new(FakeStore::default());
        let h = hash(5);
        let key = h.object_key(PREFIX);
        let two_hours_ago = Utc::now() - chrono::Duration::hours(2);
        // An old orphan from a crashed upload.
        store.insert(&key, two_hours_ago);

        let gate = FakeStore::gate(&store.put_gate);
        let uploading = tokio::spawn({
            let (repo, store) = (repo.clone(), store.clone());
            let h = h.clone();
            async move {
                repo.add(new_track(1, &h, Some(audio(10))), store.as_ref(), PREFIX)
                    .await
            }
        });
        gate.entered.notified().await; // upload holds the lock, mid-PUT

        // The sweep's listing still shows the old orphan.
        *store.listing.lock().unwrap() = Some(vec![(key.clone(), two_hours_ago)]);
        let sweeping = tokio::spawn({
            let (repo, store) = (repo.clone(), store.clone());
            async move {
                repo.sweep(store.as_ref(), PREFIX, Duration::from_secs(3600))
                    .await
            }
        });
        wait_until_blocked(&sweeping).await;

        gate.release.notify_one();
        added(uploading.await.unwrap().unwrap());
        let report = sweeping.await.unwrap().unwrap();

        assert_eq!(
            report,
            SweepReport {
                checked: 1,
                deleted: 0
            }
        );
        assert!(store.has(&key));
        assert!(!store.log().iter().any(|op| op.starts_with("delete")));
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs DATABASE_URL (make test-db)"]
    async fn pg_sweep_honours_the_grace_period(pool: PgPool) {
        let repo = PgLibrary::from_pool(pool, true);
        let store = FakeStore::default();
        let old = Utc::now() - chrono::Duration::hours(2);
        let fresh = Utc::now() - chrono::Duration::minutes(10);
        let (orphan_old, orphan_fresh, in_use) = (hash(6), hash(7), hash(8));
        store.insert(&orphan_old.object_key(PREFIX), old);
        store.insert(&orphan_fresh.object_key(PREFIX), fresh);
        store.insert("library/not-a-hash.ogg", old);
        added(add(&repo, &store, new_track(1, &in_use, Some(audio(10)))).await);
        store.insert(&in_use.object_key(PREFIX), old);
        // Listed as old, but re-stored since (fresh timestamp): kept.
        let restored = hash(9);
        store.insert(&restored.object_key(PREFIX), Utc::now());
        *store.listing.lock().unwrap() = Some(vec![
            (orphan_old.object_key(PREFIX), old),
            (orphan_fresh.object_key(PREFIX), fresh),
            ("library/not-a-hash.ogg".into(), old),
            (in_use.object_key(PREFIX), old),
            (restored.object_key(PREFIX), old),
        ]);

        let report = repo
            .sweep(&store, PREFIX, Duration::from_secs(3600))
            .await
            .unwrap();

        assert_eq!(
            report,
            SweepReport {
                checked: 3,
                deleted: 1
            }
        );
        assert!(!store.has(&orphan_old.object_key(PREFIX)));
        for kept in [&orphan_fresh, &in_use, &restored] {
            assert!(store.has(&kept.object_key(PREFIX)));
        }
        assert!(store.has("library/not-a-hash.ogg"));
    }

    /// Two uploads in one server that each fit the quota but not together:
    /// the guild lock lets exactly one through.
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs DATABASE_URL (make test-db)"]
    async fn pg_race_concurrent_uploads_respect_the_quota(pool: PgPool) {
        let repo = PgLibrary::from_pool(pool, true);
        let store = Arc::new(FakeStore::default());
        let gate = FakeStore::gate(&store.put_gate);
        let big = QUOTA * 6 / 10;

        let first = tokio::spawn({
            let (repo, store) = (repo.clone(), store.clone());
            async move {
                repo.add(
                    new_track(1, &hash(10), Some(audio(big))),
                    store.as_ref(),
                    PREFIX,
                )
                .await
            }
        });
        gate.entered.notified().await;
        let second = tokio::spawn({
            let (repo, store) = (repo.clone(), store.clone());
            async move {
                repo.add(
                    new_track(1, &hash(11), Some(audio(big))),
                    store.as_ref(),
                    PREFIX,
                )
                .await
            }
        });
        wait_until_blocked(&second).await;
        gate.release.notify_one();

        added(first.await.unwrap().unwrap());
        assert_eq!(
            second.await.unwrap().unwrap(),
            AddOutcome::QuotaExceeded { used_bytes: big }
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs DATABASE_URL (make test-db)"]
    async fn pg_search_ranks_and_escapes(pool: PgPool) {
        let repo = PgLibrary::from_pool(pool, true);
        let store = FakeStore::default();
        for (n, title) in [
            (1, "Lofi Rain Mix"),
            (2, "lofi"),
            (3, "Chill Lofi"),
            (4, "100% Pure_Jazz"),
        ] {
            let mut t = new_track(1, &hash(n), Some(audio(10)));
            t.title = title.into();
            added(add(&repo, &store, t).await);
        }
        let mut other = new_track(2, &hash(9), Some(audio(10)));
        other.title = "lofi".into();
        added(add(&repo, &store, other).await);

        let titles =
            |tracks: Vec<LibraryTrack>| tracks.into_iter().map(|t| t.title).collect::<Vec<_>>();
        assert_eq!(
            titles(repo.search(GuildId(1), "LOFI", 10).await.unwrap()),
            ["lofi", "Lofi Rain Mix", "Chill Lofi"]
        );
        // `%` and `_` are literals, not wildcards.
        assert_eq!(
            titles(repo.search(GuildId(1), "0% p", 10).await.unwrap()),
            ["100% Pure_Jazz"]
        );
        // As a wildcard, `_` would match "Pure" (u-r-e); as a literal it doesn't.
        assert!(repo.search(GuildId(1), "u_e", 10).await.unwrap().is_empty());
        assert_eq!(
            titles(repo.search(GuildId(1), "e_j", 10).await.unwrap()),
            ["100% Pure_Jazz"]
        );
        assert_eq!(repo.used_bytes(GuildId(1)).await.unwrap(), 40);
        let (page, total) = repo.list(GuildId(1), 0, 2).await.unwrap();
        assert_eq!(
            (titles(page), total),
            (
                vec!["100% Pure_Jazz".to_owned(), "Chill Lofi".to_owned()],
                4
            )
        );
    }

    async fn library_with(repo: &PgLibrary, titles: &[&str]) {
        let store = FakeStore::default();
        for (n, title) in titles.iter().enumerate() {
            let mut t = new_track(1, &hash(100 + n as u8), Some(audio(10)));
            t.title = (*title).into();
            added(add(repo, &store, t).await);
        }
    }

    async fn titles_for(repo: &PgLibrary, query: &str) -> Vec<String> {
        let tracks = repo.search(GuildId(1), query, 10).await.unwrap();
        tracks.into_iter().map(|t| t.title).collect()
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs DATABASE_URL (make test-db)"]
    async fn pg_search_is_typo_tolerant_and_ranked(pool: PgPool) {
        let repo = PgLibrary::from_pool(pool, true);
        library_with(
            &repo,
            &[
                "Sango - Never-Ending",
                "Never Gonna Give You Up",
                "Ending Theme",
                "Lofi Rain",
            ],
        )
        .await;

        // Typos and partial words, best first.
        assert_eq!(
            titles_for(&repo, "nevr endng").await[0],
            "Sango - Never-Ending"
        );
        assert_eq!(
            titles_for(&repo, "never gona").await[0],
            "Never Gonna Give You Up"
        );
        // Exact > prefix > substring > fuzzy.
        assert_eq!(
            titles_for(&repo, "ending").await[..2],
            ["Ending Theme", "Sango - Never-Ending"]
        );
        assert!(titles_for(&repo, "jazz quartet").await.is_empty());
        // Other servers' tracks never match.
        assert!(
            repo.search(GuildId(2), "lofi", 10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs DATABASE_URL (make test-db)"]
    async fn pg_search_finds_thai(pool: PgPool) {
        let repo = PgLibrary::from_pool(pool, true);
        library_with(&repo, &["เพลงรักวันฝนตก", "ลูกทุ่ง มันส์ๆ", "Thai Lofi เพลงชิล"]).await;

        // Substrings: Thai has no spaces between words.
        assert_eq!(titles_for(&repo, "รักวัน").await, ["เพลงรักวันฝนตก"]);
        // Also fuzzy-matches other titles sharing "เพลง" (song); the substring
        // match ranks first, and unrelated titles don't match at all.
        let chill = titles_for(&repo, "เพลงชิล").await;
        assert_eq!(chill[0], "Thai Lofi เพลงชิล");
        assert!(!chill.contains(&"ลูกทุ่ง มันส์ๆ".to_owned()), "{chill:?}");
        // A dropped vowel mark: fuzzy, when the locale gives Thai trigrams (it
        // does in en_US.UTF-8; see pg_search_c_locale_falls_back_to_substrings).
        assert_eq!(titles_for(&repo, "เพลงรกวนฝนตก").await[0], "เพลงรักวันฝนตก");
    }

    /// Under LC_CTYPE "C" (possible on hosted Postgres) Thai has no trigrams:
    /// substring search must still work, and fuzzy Thai simply doesn't match.
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs DATABASE_URL (make test-db)"]
    async fn pg_search_c_locale_falls_back_to_substrings(pool: PgPool) {
        // Only digits are interpolated into the SQL below.
        let name = format!("tatarbot_c_locale_{}", std::process::id());
        let sql = |text: String| sqlx::AssertSqlSafe(text);
        sqlx::query(sql(format!("DROP DATABASE IF EXISTS {name}")))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(sql(format!(
            "CREATE DATABASE {name} TEMPLATE template0 LC_COLLATE 'C' LC_CTYPE 'C'"
        )))
        .execute(&pool)
        .await
        .unwrap();
        let url = std::env::var("DATABASE_URL").unwrap();
        let base = url.rsplit_once('/').unwrap().0;
        let c_pool = PgPool::connect(&format!("{base}/{name}")).await.unwrap();
        sqlx::migrate!("./migrations").run(&c_pool).await.unwrap();
        let repo = PgLibrary::from_pool(c_pool.clone(), true);
        library_with(&repo, &["เพลงรักวันฝนตก", "Sango - Never-Ending"]).await;

        assert_eq!(titles_for(&repo, "รักวัน").await, ["เพลงรักวันฝนตก"]);
        assert!(titles_for(&repo, "เพลงรกวนฝนตก").await.is_empty());
        assert_eq!(
            titles_for(&repo, "nevr endng").await,
            ["Sango - Never-Ending"]
        );

        c_pool.close().await;
        sqlx::query(sql(format!("DROP DATABASE {name}")))
            .execute(&pool)
            .await
            .unwrap();
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs DATABASE_URL (make test-db)"]
    async fn pg_unavailable_fails_fast(pool: PgPool) {
        let repo = PgLibrary::from_pool(pool, false);
        assert!(!repo.is_available());
        assert!(matches!(
            repo.used_bytes(GuildId(1)).await,
            Err(RepoError::Unavailable)
        ));
    }
}
