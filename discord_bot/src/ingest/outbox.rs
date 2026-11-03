//! The transactional outbox (`discord_notifications`) and the upload requests it belongs to
//! (ADR 0004 §3). Rows are inserted inside the worker's commit transaction; the bot claims
//! them with a short lease, sends, and marks them delivered. A crash anywhere leaves a row
//! that is claimed again once its lease expires.

use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool, Row};

use super::notice::{Notice, nonce_for};

/// How long a claimed notification is held before another iteration may retry it. Kept
/// well inside Discord's nonce window, so a retried channel message is deduplicated.
pub const SEND_LEASE: Duration = Duration::from_secs(60);
/// Backoff between transient failures, by attempt number (1-based).
const BACKOFF_SECS: [i64; 5] = [5, 30, 120, 600, 600];
pub const MAX_ATTEMPTS: i32 = 8;
/// Give up on a notification this old.
pub const MAX_AGE: Duration = Duration::from_secs(60 * 60);

/// Inserts the notification for `request_key` in the caller's transaction. A request has
/// exactly one; a second insert is ignored.
pub async fn insert_notice(
    conn: &mut PgConnection,
    request_key: &str,
    notice: &Notice,
) -> Result<bool, sqlx::Error> {
    let outcome = serde_json::to_value(notice).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
    let done = sqlx::query(
        "INSERT INTO discord_notifications (request_key, outcome, nonce)
         SELECT $1, $2, $3 WHERE EXISTS
             (SELECT 1 FROM library_upload_requests WHERE idempotency_key = $1)
         ON CONFLICT (request_key) DO NOTHING",
    )
    .bind(request_key)
    .bind(outcome)
    .bind(nonce_for(request_key))
    .execute(conn)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// A claimed notification plus what is needed to deliver it.
#[derive(Debug, Clone)]
pub struct Claimed {
    pub id: i64,
    pub request_key: String,
    pub notice: Notice,
    pub nonce: String,
    pub attempts: i32,
    pub created_at: DateTime<Utc>,
    pub channel_id: u64,
    pub user_id: u64,
    /// Present only while it is valid for at least another 30 s.
    pub interaction_token: Option<String>,
}

/// Claims the next due notification (pending and due, or sending with an expired lease).
pub async fn claim(pool: &PgPool) -> Result<Option<Claimed>, sqlx::Error> {
    let row = sqlx::query(
        "WITH next AS (
             SELECT id FROM discord_notifications
             WHERE (state = 'pending' AND next_attempt_at <= now())
                OR (state = 'sending' AND lease_expires_at < now())
             ORDER BY id FOR UPDATE SKIP LOCKED LIMIT 1)
         UPDATE discord_notifications n SET state = 'sending', attempts = n.attempts + 1,
             lease_expires_at = now() + make_interval(secs => $1)
         FROM next, library_upload_requests r
         WHERE n.id = next.id AND r.idempotency_key = n.request_key
         RETURNING n.id, n.request_key, n.outcome, n.nonce, n.attempts, n.created_at,
             r.channel_id, r.user_id,
             CASE WHEN r.token_expires_at > now() + interval '30 seconds'
                  THEN r.interaction_token END AS token",
    )
    .bind(SEND_LEASE.as_secs_f64())
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else { return Ok(None) };
    let outcome: serde_json::Value = row.try_get("outcome")?;
    let notice = serde_json::from_value(outcome).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
    Ok(Some(Claimed {
        id: row.try_get("id")?,
        request_key: row.try_get("request_key")?,
        notice,
        nonce: row.try_get("nonce")?,
        attempts: row.try_get("attempts")?,
        created_at: row.try_get("created_at")?,
        channel_id: row.try_get::<i64, _>("channel_id")? as u64,
        user_id: row.try_get::<i64, _>("user_id")? as u64,
        interaction_token: row.try_get("token")?,
    }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    Interaction,
    Channel,
}

/// Marks a claimed notification delivered and clears the interaction token.
pub async fn delivered(
    pool: &PgPool,
    c: &Claimed,
    via: Via,
    message_id: Option<u64>,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "UPDATE discord_notifications SET state = 'delivered', delivered_via = $2,
             discord_message_id = $3, delivered_at = now(), lease_expires_at = NULL
         WHERE id = $1 AND state = 'sending'",
    )
    .bind(c.id)
    .bind(match via {
        Via::Interaction => "interaction",
        Via::Channel => "channel",
    })
    .bind(message_id.map(|m| m as i64))
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE library_upload_requests SET interaction_token = NULL WHERE idempotency_key = $1",
    )
    .bind(&c.request_key)
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}

/// Back to pending after a transient failure, or undeliverable once attempts or age run out.
pub async fn retry_later(pool: &PgPool, c: &Claimed, error: &str) -> Result<bool, sqlx::Error> {
    let age = Utc::now() - c.created_at;
    if c.attempts >= MAX_ATTEMPTS || age.to_std().is_ok_and(|a| a > MAX_AGE) {
        undeliverable(pool, c, error).await?;
        return Ok(false);
    }
    let idx = (c.attempts.max(1) as usize - 1).min(BACKOFF_SECS.len() - 1);
    sqlx::query(
        "UPDATE discord_notifications SET state = 'pending', lease_expires_at = NULL,
             next_attempt_at = now() + make_interval(secs => $2), last_error = $3
         WHERE id = $1 AND state = 'sending'",
    )
    .bind(c.id)
    .bind(BACKOFF_SECS[idx] as f64)
    .bind(error.chars().take(1000).collect::<String>())
    .execute(pool)
    .await?;
    Ok(true)
}

pub async fn undeliverable(pool: &PgPool, c: &Claimed, error: &str) -> Result<(), sqlx::Error> {
    tracing::warn!(
        notification = c.id,
        error,
        "upload notification undeliverable"
    );
    sqlx::query(
        "UPDATE discord_notifications SET state = 'undeliverable', lease_expires_at = NULL,
             last_error = $2
         WHERE id = $1 AND state = 'sending'",
    )
    .bind(c.id)
    .bind(error.chars().take(1000).collect::<String>())
    .execute(pool)
    .await?;
    Ok(())
}

/// Housekeeping: drop expired tokens; delete requests (and their notifications) after 30 days.
pub async fn housekeeping(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE library_upload_requests SET interaction_token = NULL
         WHERE interaction_token IS NOT NULL AND token_expires_at < now()",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "DELETE FROM library_upload_requests WHERE created_at < now() - interval '30 days'",
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// What the consumer needs from the request row.
#[derive(Debug, Clone)]
pub struct RequestRow {
    pub title: String,
    pub guild_id: u64,
    pub user_id: u64,
}

pub async fn request(
    conn: &mut PgConnection,
    key: &str,
) -> Result<Option<RequestRow>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT title, guild_id, user_id FROM library_upload_requests WHERE idempotency_key = $1",
    )
    .bind(key)
    .fetch_optional(conn)
    .await?;
    row.map(|r| {
        Ok(RequestRow {
            title: r.try_get("title")?,
            guild_id: r.try_get::<i64, _>("guild_id")? as u64,
            user_id: r.try_get::<i64, _>("user_id")? as u64,
        })
    })
    .transpose()
}
