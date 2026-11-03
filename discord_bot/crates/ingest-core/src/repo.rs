//! `ingest.*` persistence. Every state change is one statement or one transaction with a
//! fence, so concurrent deliveries, stale attempts and the reaper can't double-apply.

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

use crate::model::{Job, JobError, JobState, Producer, Source, SourceKind, SubmitRequest};

/// Advisory lock space for (producer, tenant) admission checks (two-int form).
const ADMISSION_LOCK_SPACE: i32 = 0x1A6E;

#[derive(Debug, thiserror::Error)]
pub enum RepoError {
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
    #[error("corrupt row: {0}")]
    Corrupt(String),
}

/// Why a submission was refused by the admission limits.
#[derive(Debug, Clone, PartialEq)]
pub enum Admission {
    /// Inserted now.
    Created(Job),
    /// The idempotency key already existed: the original job.
    Existing(Job),
    TooManyActive {
        limit: u32,
    },
    TenantRateLimited {
        limit: u32,
    },
}

#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ReapReport {
    pub requeued_expired: u64,
    pub failed_exhausted: u64,
    pub timed_out: u64,
    pub enqueued: u64,
}

#[derive(Clone)]
pub struct Repo {
    pool: PgPool,
}

/// The `ingest.jobs` columns `job_from_row` reads (a macro so queries stay `&'static str`).
macro_rules! job_columns {
    () => {
        "id, producer_id, idempotency_key, tenant, actor, pipeline, params, source_kind, \
         source_uri, declared_size, state, attempts, max_attempts, lease_owner, lease_expires_at, \
         cancel_requested, source_sha256, result, error_code, error_detail, enqueued_at, \
         enqueue_generation, created_at, updated_at, finished_at"
    };
}

fn job_from_row(row: &PgRow) -> Result<Job, RepoError> {
    let state: String = row.try_get("state")?;
    let kind: String = row.try_get("source_kind")?;
    let declared: Option<i64> = row.try_get("declared_size")?;
    Ok(Job {
        id: row.try_get("id")?,
        producer_id: row.try_get("producer_id")?,
        idempotency_key: row.try_get("idempotency_key")?,
        tenant: row.try_get("tenant")?,
        actor: row.try_get("actor")?,
        pipeline: row.try_get("pipeline")?,
        params: row.try_get("params")?,
        source: Source {
            kind: SourceKind::parse(&kind).ok_or_else(|| RepoError::Corrupt(kind.clone()))?,
            uri: row.try_get("source_uri")?,
            declared_size: declared.map(|d| d.max(0) as u64),
        },
        state: JobState::parse(&state).ok_or_else(|| RepoError::Corrupt(state.clone()))?,
        attempts: row.try_get("attempts")?,
        max_attempts: row.try_get("max_attempts")?,
        lease_owner: row.try_get("lease_owner")?,
        lease_expires_at: row.try_get("lease_expires_at")?,
        cancel_requested: row.try_get("cancel_requested")?,
        source_sha256: row.try_get("source_sha256")?,
        result: row.try_get("result")?,
        error_code: row.try_get("error_code")?,
        error_detail: row.try_get("error_detail")?,
        enqueued_at: row.try_get("enqueued_at")?,
        enqueue_generation: row.try_get("enqueue_generation")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
        finished_at: row.try_get("finished_at")?,
    })
}

fn producer_from_row(row: &PgRow) -> Result<Producer, RepoError> {
    Ok(Producer {
        id: row.try_get("id")?,
        service_account: row.try_get("service_account")?,
        tenant_prefix: row.try_get("tenant_prefix")?,
        allowed_pipelines: row.try_get("allowed_pipelines")?,
        allowed_source_kinds: row.try_get("allowed_source_kinds")?,
        source_url_hosts: row.try_get("source_url_hosts")?,
        max_source_bytes: row.try_get::<i64, _>("max_source_bytes")?.max(0) as u64,
        max_active_per_actor: row.try_get::<i32, _>("max_active_per_actor")?.max(0) as u32,
        max_jobs_per_tenant_hour: row.try_get::<i32, _>("max_jobs_per_tenant_hour")?.max(0) as u32,
        enabled: row.try_get("enabled")?,
    })
}

fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

impl Repo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    // ---- producers

    pub async fn upsert_producer(&self, p: &Producer) -> Result<(), RepoError> {
        sqlx::query(
            "INSERT INTO ingest.producers (id, service_account, tenant_prefix, allowed_pipelines,
                 allowed_source_kinds, source_url_hosts, max_source_bytes, max_active_per_actor,
                 max_jobs_per_tenant_hour, enabled)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
             ON CONFLICT (id) DO UPDATE SET service_account = EXCLUDED.service_account,
                 tenant_prefix = EXCLUDED.tenant_prefix,
                 allowed_pipelines = EXCLUDED.allowed_pipelines,
                 allowed_source_kinds = EXCLUDED.allowed_source_kinds,
                 source_url_hosts = EXCLUDED.source_url_hosts,
                 max_source_bytes = EXCLUDED.max_source_bytes,
                 max_active_per_actor = EXCLUDED.max_active_per_actor,
                 max_jobs_per_tenant_hour = EXCLUDED.max_jobs_per_tenant_hour,
                 enabled = EXCLUDED.enabled, updated_at = now()",
        )
        .bind(&p.id)
        .bind(&p.service_account)
        .bind(&p.tenant_prefix)
        .bind(&p.allowed_pipelines)
        .bind(&p.allowed_source_kinds)
        .bind(&p.source_url_hosts)
        .bind(p.max_source_bytes as i64)
        .bind(p.max_active_per_actor as i32)
        .bind(p.max_jobs_per_tenant_hour as i32)
        .bind(p.enabled)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn producer(&self, id: &str) -> Result<Option<Producer>, RepoError> {
        let row = sqlx::query("SELECT * FROM ingest.producers WHERE id = $1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(producer_from_row).transpose()
    }

    pub async fn producer_by_account(&self, email: &str) -> Result<Option<Producer>, RepoError> {
        let row = sqlx::query("SELECT * FROM ingest.producers WHERE service_account = $1")
            .bind(email)
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(producer_from_row).transpose()
    }

    // ---- jobs

    pub async fn job(&self, id: Uuid) -> Result<Option<Job>, RepoError> {
        let row = sqlx::query(concat!(
            "SELECT ",
            job_columns!(),
            " FROM ingest.jobs WHERE id = $1"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(job_from_row).transpose()
    }

    /// Idempotent by (producer, key); admission limits are checked under a per-tenant lock so
    /// concurrent submissions can't both slip under a limit. Policy is checked by the caller.
    pub async fn submit(
        &self,
        producer: &Producer,
        req: &SubmitRequest,
        max_attempts: i32,
    ) -> Result<Admission, RepoError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock($1, hashtext($2))")
            .bind(ADMISSION_LOCK_SPACE)
            .bind(format!("{}/{}", producer.id, req.tenant))
            .execute(&mut *tx)
            .await?;
        if let Some(row) = sqlx::query(concat!(
            "SELECT ",
            job_columns!(),
            " FROM ingest.jobs WHERE producer_id = $1 AND idempotency_key = $2"
        ))
        .bind(&producer.id)
        .bind(&req.idempotency_key)
        .fetch_optional(&mut *tx)
        .await?
        {
            return Ok(Admission::Existing(job_from_row(&row)?));
        }
        let active: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM ingest.jobs WHERE producer_id = $1 AND actor = $2
               AND state IN ('queued', 'processing')",
        )
        .bind(&producer.id)
        .bind(&req.actor)
        .fetch_one(&mut *tx)
        .await?;
        if active >= i64::from(producer.max_active_per_actor) {
            return Ok(Admission::TooManyActive {
                limit: producer.max_active_per_actor,
            });
        }
        let recent: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM ingest.jobs WHERE producer_id = $1 AND tenant = $2
               AND created_at > now() - interval '1 hour'",
        )
        .bind(&producer.id)
        .bind(&req.tenant)
        .fetch_one(&mut *tx)
        .await?;
        if recent >= i64::from(producer.max_jobs_per_tenant_hour) {
            return Ok(Admission::TenantRateLimited {
                limit: producer.max_jobs_per_tenant_hour,
            });
        }
        let row = sqlx::query(concat!(
            "INSERT INTO ingest.jobs (producer_id, idempotency_key, tenant, actor, pipeline,
                 params, source_kind, source_uri, declared_size, max_attempts)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
             RETURNING ",
            job_columns!(),
            ""
        ))
        .bind(&producer.id)
        .bind(&req.idempotency_key)
        .bind(&req.tenant)
        .bind(&req.actor)
        .bind(&req.pipeline)
        .bind(&req.params)
        .bind(req.source.kind.as_str())
        .bind(&req.source.uri)
        .bind(req.source.declared_size.map(|s| s as i64))
        .bind(max_attempts)
        .fetch_one(&mut *tx)
        .await?;
        let job = job_from_row(&row)?;
        tx.commit().await?;
        Ok(Admission::Created(job))
    }

    /// Records that a task for `generation` exists.
    pub async fn mark_enqueued(&self, id: Uuid, generation: i32) -> Result<(), RepoError> {
        sqlx::query(
            "UPDATE ingest.jobs SET enqueued_at = now(), updated_at = now()
             WHERE id = $1 AND state = 'queued' AND enqueue_generation = $2",
        )
        .bind(id)
        .bind(generation)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The job needs a (new) task: the reaper creates it.
    pub async fn mark_needs_enqueue(&self, id: Uuid) -> Result<(), RepoError> {
        sqlx::query(
            "UPDATE ingest.jobs SET enqueued_at = NULL, updated_at = now()
             WHERE id = $1 AND state = 'queued'",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// queued → processing (or takeover of an expired lease). `None` if not claimable.
    pub async fn claim(
        &self,
        id: Uuid,
        owner: Uuid,
        lease_secs: i64,
    ) -> Result<Option<Job>, RepoError> {
        let row = sqlx::query(concat!(
            "UPDATE ingest.jobs SET state = 'processing', attempts = attempts + 1,
                 lease_owner = $2, lease_expires_at = now() + make_interval(secs => $3),
                 updated_at = now()
             WHERE id = $1 AND NOT cancel_requested AND attempts < max_attempts
               AND (state = 'queued' OR (state = 'processing' AND lease_expires_at < now()))
             RETURNING ",
            job_columns!(),
            ""
        ))
        .bind(id)
        .bind(owner)
        .bind(lease_secs as f64)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(job_from_row).transpose()
    }

    /// processing → queued after a transient failure; only the lease holder can.
    pub async fn release(&self, id: Uuid, owner: Uuid, error: &str) -> Result<bool, RepoError> {
        let done = sqlx::query(
            "UPDATE ingest.jobs SET state = 'queued', lease_owner = NULL, lease_expires_at = NULL,
                 last_error = $3, updated_at = now()
             WHERE id = $1 AND lease_owner = $2 AND state = 'processing'",
        )
        .bind(id)
        .bind(owner)
        .bind(truncate(error, 1000))
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() == 1)
    }

    pub async fn set_source_sha256(
        &self,
        id: Uuid,
        owner: Uuid,
        sha256: &str,
    ) -> Result<(), RepoError> {
        sqlx::query(
            "UPDATE ingest.jobs SET source_sha256 = $3, updated_at = now()
             WHERE id = $1 AND lease_owner = $2",
        )
        .bind(id)
        .bind(owner)
        .bind(sha256)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn cancel_requested(&self, id: Uuid) -> Result<bool, RepoError> {
        Ok(
            sqlx::query_scalar("SELECT cancel_requested FROM ingest.jobs WHERE id = $1")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?
                .unwrap_or(false),
        )
    }

    /// Sets the cancel flag on an unfinished job of `producer`; returns the job afterwards.
    pub async fn request_cancel(&self, id: Uuid, producer: &str) -> Result<Option<Job>, RepoError> {
        let row = sqlx::query(concat!(
            "UPDATE ingest.jobs SET cancel_requested = true, updated_at = now()
             WHERE id = $1 AND producer_id = $2 AND state IN ('queued', 'processing')
             RETURNING ",
            job_columns!(),
            ""
        ))
        .bind(id)
        .bind(producer)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(job_from_row).transpose()
    }

    // ---- fenced transitions (inside a caller's transaction)

    /// Locks the job if `owner` still holds its lease.
    pub async fn fence(
        conn: &mut PgConnection,
        id: Uuid,
        owner: Uuid,
    ) -> Result<Option<Job>, RepoError> {
        let row = sqlx::query(concat!(
            "SELECT ",
            job_columns!(),
            " FROM ingest.jobs
             WHERE id = $1 AND state = 'processing' AND lease_owner = $2 FOR UPDATE"
        ))
        .bind(id)
        .bind(owner)
        .fetch_optional(&mut *conn)
        .await?;
        row.as_ref().map(job_from_row).transpose()
    }

    /// Locks the job if it is in `state` (for transitions not owned by a lease: cancelling a
    /// queued job, the reaper).
    pub async fn lock_in_state(
        conn: &mut PgConnection,
        id: Uuid,
        state: JobState,
    ) -> Result<Option<Job>, RepoError> {
        let row = sqlx::query(concat!(
            "SELECT ",
            job_columns!(),
            " FROM ingest.jobs WHERE id = $1 AND state = $2 FOR UPDATE"
        ))
        .bind(id)
        .bind(state.as_str())
        .fetch_optional(&mut *conn)
        .await?;
        row.as_ref().map(job_from_row).transpose()
    }

    /// Terminal transition + event, on a row the caller has locked.
    pub async fn finish(
        conn: &mut PgConnection,
        id: Uuid,
        state: JobState,
        result: Option<&Value>,
        error: Option<&JobError>,
    ) -> Result<(), RepoError> {
        debug_assert!(state.is_terminal());
        sqlx::query(
            "UPDATE ingest.jobs SET state = $2, result = $3, error_code = $4, error_detail = $5,
                 lease_owner = NULL, lease_expires_at = NULL, finished_at = now(),
                 updated_at = now()
             WHERE id = $1",
        )
        .bind(id)
        .bind(state.as_str())
        .bind(result)
        .bind(error.map(|e| truncate(&e.code, 64)))
        .bind(
            error
                .and_then(|e| e.detail.as_deref())
                .map(|d| truncate(d, 500)),
        )
        .execute(&mut *conn)
        .await?;
        sqlx::query("INSERT INTO ingest.job_events (job_id, type) VALUES ($1, $2)")
            .bind(id)
            .bind(state.as_str())
            .execute(&mut *conn)
            .await?;
        Ok(())
    }

    // ---- kill switch

    /// The first paused scope that applies to (producer, pipeline), with its reason.
    pub async fn paused(
        &self,
        producer: &str,
        pipeline: &str,
    ) -> Result<Option<(String, String)>, RepoError> {
        let row = sqlx::query(
            "SELECT scope, reason FROM ingest.switches WHERE paused
               AND scope IN ('global', 'producer:' || $1, 'pipeline:' || $2)
             ORDER BY scope = 'global' DESC LIMIT 1",
        )
        .bind(producer)
        .bind(pipeline)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| (r.get(0), r.get(1))))
    }

    pub async fn set_switch(
        &self,
        scope: &str,
        paused: bool,
        reason: &str,
        by: &str,
    ) -> Result<(), RepoError> {
        sqlx::query(
            "INSERT INTO ingest.switches (scope, paused, reason, updated_by) VALUES ($1, $2, $3, $4)
             ON CONFLICT (scope) DO UPDATE SET paused = EXCLUDED.paused, reason = EXCLUDED.reason,
                 updated_by = EXCLUDED.updated_by, updated_at = now()",
        )
        .bind(scope)
        .bind(paused)
        .bind(reason)
        .bind(by)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ---- reaper queries

    /// Processing jobs whose lease expired (their attempt died).
    pub async fn expired_leases(&self, limit: i64) -> Result<Vec<Uuid>, RepoError> {
        Ok(sqlx::query_scalar(
            "SELECT id FROM ingest.jobs WHERE state = 'processing' AND lease_expires_at < now()
             ORDER BY lease_expires_at LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// processing (expired) → queued, needing a task. False if something else won.
    pub async fn requeue_expired(conn: &mut PgConnection, id: Uuid) -> Result<bool, RepoError> {
        let done = sqlx::query(
            "UPDATE ingest.jobs SET state = 'queued', lease_owner = NULL, lease_expires_at = NULL,
                 enqueued_at = NULL, last_error = 'lease expired', updated_at = now()
             WHERE id = $1 AND state = 'processing' AND lease_expires_at < now()",
        )
        .bind(id)
        .execute(&mut *conn)
        .await?;
        Ok(done.rows_affected() == 1)
    }

    /// Queued jobs untouched for `older_than_secs`, outside paused scopes.
    pub async fn stale_queued(
        &self,
        older_than_secs: i64,
        limit: i64,
    ) -> Result<Vec<Uuid>, RepoError> {
        Ok(sqlx::query_scalar(
            "SELECT j.id FROM ingest.jobs j
             WHERE j.state = 'queued' AND j.updated_at < now() - make_interval(secs => $1)
               AND NOT EXISTS (SELECT 1 FROM ingest.switches s WHERE s.paused
                   AND s.scope IN ('global', 'producer:' || j.producer_id, 'pipeline:' || j.pipeline))
             ORDER BY j.updated_at LIMIT $2",
        )
        .bind(older_than_secs as f64)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Takes the next generation for queued jobs needing a task (none yet, or the old one is
    /// surely dead), outside paused scopes. Returns (job, generation) to enqueue.
    pub async fn take_for_enqueue(
        &self,
        dead_after_secs: i64,
        limit: i64,
    ) -> Result<Vec<(Uuid, i32)>, RepoError> {
        let rows = sqlx::query(
            "UPDATE ingest.jobs j SET enqueue_generation = j.enqueue_generation + 1,
                 enqueued_at = now(), updated_at = now()
             WHERE j.id IN (
                 SELECT j2.id FROM ingest.jobs j2
                 WHERE j2.state = 'queued' AND NOT j2.cancel_requested
                   AND (j2.enqueued_at IS NULL
                        OR j2.enqueued_at < now() - make_interval(secs => $1))
                   AND NOT EXISTS (SELECT 1 FROM ingest.switches s WHERE s.paused
                       AND s.scope IN ('global', 'producer:' || j2.producer_id,
                                       'pipeline:' || j2.pipeline))
                 ORDER BY j2.created_at LIMIT $2
                 FOR UPDATE SKIP LOCKED)
             RETURNING j.id, j.enqueue_generation",
        )
        .bind(dead_after_secs as f64)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(|r| (r.get(0), r.get(1))).collect())
    }

    /// Queued jobs with the cancel flag (cancelled before any attempt claimed them).
    pub async fn cancelled_queued(&self, limit: i64) -> Result<Vec<Uuid>, RepoError> {
        Ok(sqlx::query_scalar(
            "SELECT id FROM ingest.jobs WHERE state = 'queued' AND cancel_requested LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Deletes finished jobs (and their events) older than `days`.
    pub async fn purge_finished(&self, days: i32) -> Result<u64, RepoError> {
        let done = sqlx::query(
            "DELETE FROM ingest.jobs WHERE finished_at < now() - make_interval(days => $1)",
        )
        .bind(days)
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected())
    }

    pub async fn now(&self) -> Result<DateTime<Utc>, RepoError> {
        Ok(sqlx::query_scalar("SELECT now()")
            .fetch_one(&self.pool)
            .await?)
    }
}
