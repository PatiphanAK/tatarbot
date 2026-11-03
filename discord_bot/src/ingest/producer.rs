//! TatarBot as an ingest producer (ADR 0004): records the upload request, submits the job
//! (`POST /v1/jobs`, idempotent by interaction id) and re-submits what didn't go through.

use std::path::PathBuf;
use std::time::Duration;

use ingest_core::{JobView, Source, SourceKind, SubmitRequest};
use serde::Deserialize;
use serde_json::json;
use sqlx::{PgPool, Row};
use url::Url;

use super::consumer::{actor, tenant};
use super::notice::Notice;
use super::outbox;
use super::pipeline::PIPELINE_ID;
use crate::application::ports::Attachment;
use crate::domain::library::MAX_DURATION;
use crate::domain::music::{GuildId, UserId};

/// Give up re-submitting after this long (the member is told).
const RESUBMIT_FOR: Duration = Duration::from_secs(10 * 60);
const INTERACTION_TOKEN_TTL: Duration = Duration::from_secs(15 * 60);

/// Talks to the worker's job API with the ID token the host writes to `token_file`.
#[derive(Clone)]
pub struct IngestClient {
    base: Url,
    token_file: PathBuf,
    http: reqwest::Client,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitFailure {
    /// The worker answered and said no (policy, limits, paused). Retrying won't help soon.
    Refused { status: u16, code: String },
    /// Couldn't reach it, or it failed: retry later.
    Unavailable(String),
}

#[derive(Deserialize)]
struct ApiError {
    error: String,
}

impl IngestClient {
    pub fn new(base: Url, token_file: PathBuf, timeout: Duration) -> Result<Self, reqwest::Error> {
        Ok(Self {
            base,
            token_file,
            http: reqwest::Client::builder().timeout(timeout).build()?,
        })
    }

    /// `Authorization`, not `X-Serverless-Authorization`: Cloud Run strips the signature from
    /// the latter, and the worker verifies the token itself.
    async fn bearer(&self) -> Result<String, SubmitFailure> {
        super::gcs::read_token(&self.token_file)
            .await
            .map(|t| format!("Bearer {t}"))
            .map_err(|e| SubmitFailure::Unavailable(e.to_string()))
    }

    pub async fn submit(&self, req: &SubmitRequest) -> Result<JobView, SubmitFailure> {
        let url = self
            .base
            .join("v1/jobs")
            .map_err(|e| SubmitFailure::Unavailable(e.to_string()))?;
        let response = self
            .http
            .post(url)
            .header(reqwest::header::AUTHORIZATION, self.bearer().await?)
            .json(req)
            .send()
            .await
            .map_err(|e| SubmitFailure::Unavailable(e.to_string()))?;
        let status = response.status().as_u16();
        match status {
            200 | 201 => response
                .json()
                .await
                .map_err(|e| SubmitFailure::Unavailable(e.to_string())),
            400 | 403 | 429 | 503 => {
                let code = response
                    .json::<ApiError>()
                    .await
                    .map(|e| e.error)
                    .unwrap_or_else(|_| format!("http_{status}"));
                // 401/5xx without a code (or keys unavailable) mean "try again".
                if status == 503 && code != "paused" {
                    return Err(SubmitFailure::Unavailable(code));
                }
                Err(SubmitFailure::Refused { status, code })
            }
            _ => Err(SubmitFailure::Unavailable(format!("HTTP {status}"))),
        }
    }
}

/// One `/upload` the bot accepted.
#[derive(Debug, Clone)]
pub struct UploadContext {
    pub interaction_id: u64,
    pub application_id: u64,
    pub token: String,
    pub guild: GuildId,
    pub channel_id: u64,
    pub user: UserId,
    pub title: String,
    pub attachment: Attachment,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Queued {
    Submitted,
    /// Recorded; the worker was unreachable, re-submission continues in the background.
    WillRetry,
}

pub struct UploadQueue {
    pool: PgPool,
    client: IngestClient,
}

struct PendingRow {
    key: String,
    guild_id: u64,
    user_id: u64,
    attachment_url: String,
    declared_size: u64,
    created_at: chrono::DateTime<chrono::Utc>,
}

pub fn idempotency_key(interaction_id: u64) -> String {
    format!("discord-interaction:{interaction_id}")
}

fn submit_request(row: &PendingRow) -> SubmitRequest {
    SubmitRequest {
        idempotency_key: row.key.clone(),
        tenant: tenant(GuildId(row.guild_id)),
        actor: actor(UserId(row.user_id)),
        pipeline: PIPELINE_ID.into(),
        params: json!({ "max_duration_s": MAX_DURATION.as_secs() }),
        source: Source {
            kind: SourceKind::HttpsUrl,
            uri: row.attachment_url.clone(),
            declared_size: Some(row.declared_size),
        },
    }
}

impl UploadQueue {
    pub fn new(pool: PgPool, client: IngestClient) -> Self {
        Self { pool, client }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Records the request, then submits it. A refusal deletes the request and is returned
    /// for the member; an unreachable worker leaves it for the re-submitter.
    pub async fn enqueue(&self, ctx: &UploadContext) -> Result<Queued, SubmitFailure> {
        let key = idempotency_key(ctx.interaction_id);
        sqlx::query(
            "INSERT INTO library_upload_requests (idempotency_key, application_id, guild_id,
                 channel_id, user_id, title, attachment_url, declared_size, interaction_token,
                 token_expires_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, now() + make_interval(secs => $10))
             ON CONFLICT (idempotency_key) DO NOTHING",
        )
        .bind(&key)
        .bind(ctx.application_id as i64)
        .bind(ctx.guild.0 as i64)
        .bind(ctx.channel_id as i64)
        .bind(ctx.user.0 as i64)
        .bind(&ctx.title)
        .bind(&ctx.attachment.url)
        .bind(ctx.attachment.declared_size as i64)
        .bind(&ctx.token)
        .bind(INTERACTION_TOKEN_TTL.as_secs_f64())
        .execute(&self.pool)
        .await
        .map_err(|e| SubmitFailure::Unavailable(e.to_string()))?;
        let row = PendingRow {
            key: key.clone(),
            guild_id: ctx.guild.0,
            user_id: ctx.user.0,
            attachment_url: ctx.attachment.url.clone(),
            declared_size: ctx.attachment.declared_size,
            created_at: chrono::Utc::now(),
        };
        match self.submit_row(&row).await {
            Ok(()) => Ok(Queued::Submitted),
            Err(SubmitFailure::Unavailable(reason)) => {
                tracing::warn!(key, reason, "ingest worker unreachable; will re-submit");
                Ok(Queued::WillRetry)
            }
            Err(refused) => {
                sqlx::query("DELETE FROM library_upload_requests WHERE idempotency_key = $1")
                    .bind(&key)
                    .execute(&self.pool)
                    .await
                    .map_err(|e| SubmitFailure::Unavailable(e.to_string()))?;
                Err(refused)
            }
        }
    }

    async fn submit_row(&self, row: &PendingRow) -> Result<(), SubmitFailure> {
        let result = self.client.submit(&submit_request(row)).await;
        let job_id = result.as_ref().ok().map(|view| view.job_id);
        sqlx::query(
            "UPDATE library_upload_requests SET job_id = COALESCE($2, job_id),
                 submit_attempts = submit_attempts + 1
             WHERE idempotency_key = $1",
        )
        .bind(&row.key)
        .bind(job_id)
        .execute(&self.pool)
        .await
        .map_err(|e| SubmitFailure::Unavailable(e.to_string()))?;
        result.map(|_| ())
    }

    /// Re-submits requests that never reached the worker; gives up after `RESUBMIT_FOR`
    /// (or on a refusal) with a notification the member will see.
    pub async fn resubmit_pending(&self) -> Result<usize, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT idempotency_key, guild_id, user_id, attachment_url, declared_size, created_at
             FROM library_upload_requests WHERE job_id IS NULL AND abandoned_at IS NULL
             ORDER BY created_at LIMIT 50",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut submitted = 0;
        for r in rows {
            let row = PendingRow {
                key: r.try_get("idempotency_key")?,
                guild_id: r.try_get::<i64, _>("guild_id")? as u64,
                user_id: r.try_get::<i64, _>("user_id")? as u64,
                attachment_url: r.try_get("attachment_url")?,
                declared_size: r.try_get::<i64, _>("declared_size")?.max(1) as u64,
                created_at: r.try_get("created_at")?,
            };
            let expired = (chrono::Utc::now() - row.created_at)
                .to_std()
                .is_ok_and(|age| age > RESUBMIT_FOR);
            let notice = if expired {
                Some(Notice::SubmissionFailed)
            } else {
                match self.submit_row(&row).await {
                    Ok(()) => {
                        submitted += 1;
                        None
                    }
                    Err(SubmitFailure::Unavailable(_)) => None,
                    Err(SubmitFailure::Refused { code, .. }) => Some(Notice::Failed {
                        code,
                        data: serde_json::Value::Null,
                    }),
                }
            };
            if let Some(notice) = notice {
                let mut tx = self.pool.begin().await?;
                sqlx::query(
                    "UPDATE library_upload_requests SET abandoned_at = now()
                     WHERE idempotency_key = $1 AND job_id IS NULL",
                )
                .bind(&row.key)
                .execute(&mut *tx)
                .await?;
                outbox::insert_notice(&mut tx, &row.key, &notice).await?;
                tx.commit().await?;
            }
        }
        Ok(submitted)
    }
}

/// Background loop: re-submission every 30 s.
pub async fn run_resubmitter(queue: std::sync::Arc<UploadQueue>) {
    let mut tick = tokio::time::interval(Duration::from_secs(30));
    loop {
        tick.tick().await;
        if let Err(err) = queue.resubmit_pending().await {
            tracing::warn!(%err, "upload re-submission failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::post;
    use axum::{Json, Router};
    use serde_json::{Value, json};
    use sqlx::PgPool;

    use super::*;

    /// A fake job API answering from a script; records Authorization headers and bodies.
    #[derive(Default)]
    struct Api {
        seen: Mutex<Vec<(String, Value)>>,
        script: Mutex<Vec<(u16, Value)>>,
    }

    async fn serve(api: Arc<Api>) -> Url {
        let router =
            Router::new()
                .route(
                    "/v1/jobs",
                    post(
                        |State(api): State<Arc<Api>>,
                         headers: HeaderMap,
                         Json(body): Json<Value>| async move {
                            let auth = headers
                                .get("authorization")
                                .map(|v| v.to_str().unwrap().to_owned())
                                .unwrap_or_default();
                            api.seen.lock().unwrap().push((auth, body));
                            let (status, body) = api.script.lock().unwrap().pop().unwrap_or((
                            201,
                            json!({
                                "job_id": uuid::Uuid::new_v4(), "state": "queued", "attempts": 0,
                                "result": null, "error_code": null, "error_detail": null,
                                "created_at": chrono::Utc::now(), "finished_at": null
                            }),
                        ));
                            (StatusCode::from_u16(status).unwrap(), Json(body))
                        },
                    ),
                )
                .with_state(api);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        url
    }

    fn ctx(interaction: u64) -> UploadContext {
        UploadContext {
            interaction_id: interaction,
            application_id: 1,
            token: "interaction-token".into(),
            guild: GuildId(10),
            channel_id: 20,
            user: UserId(30),
            title: "Song".into(),
            attachment: Attachment {
                url: "https://cdn.discordapp.com/attachments/1/2/song.mp3".into(),
                filename: "song.mp3".into(),
                declared_size: 1234,
            },
        }
    }

    async fn queue(pool: PgPool, api: Arc<Api>) -> (UploadQueue, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let token = dir.path().join("ingest-id-token");
        tokio::fs::write(&token, "eyJ.id.token\n").await.unwrap();
        let client = IngestClient::new(serve(api).await, token, Duration::from_secs(5)).unwrap();
        (UploadQueue::new(pool, client), dir)
    }

    async fn job_id(pool: &PgPool, key: &str) -> Option<Option<uuid::Uuid>> {
        sqlx::query_scalar("SELECT job_id FROM library_upload_requests WHERE idempotency_key = $1")
            .bind(key)
            .fetch_optional(pool)
            .await
            .unwrap()
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs DATABASE_URL (make test-db)"]
    async fn pg_upload_submits_with_the_id_token_and_a_generic_contract(pool: PgPool) {
        let api = Arc::new(Api::default());
        let (q, _dir) = queue(pool.clone(), api.clone()).await;
        assert_eq!(q.enqueue(&ctx(7)).await.unwrap(), Queued::Submitted);
        let (auth, body) = api.seen.lock().unwrap()[0].clone();
        assert_eq!(auth, "Bearer eyJ.id.token");
        assert_eq!(body["idempotency_key"], "discord-interaction:7");
        assert_eq!(body["tenant"], "discord:guild:10");
        assert_eq!(body["actor"], "discord:user:30");
        assert_eq!(body["pipeline"], "audio.opus-v1");
        assert_eq!(body["source"]["kind"], "https_url");
        // The job API sees no interaction token, channel or title.
        let text = body.to_string();
        assert!(!text.contains("interaction-token") && !text.contains("Song"));
        assert!(
            job_id(&pool, "discord-interaction:7")
                .await
                .unwrap()
                .is_some()
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs DATABASE_URL (make test-db)"]
    async fn pg_upload_refusal_is_returned_and_forgotten(pool: PgPool) {
        let api = Arc::new(Api::default());
        api.script.lock().unwrap().push((
            503,
            json!({ "error": "paused", "detail": "global: budget" }),
        ));
        let (q, _dir) = queue(pool.clone(), api).await;
        assert_eq!(
            q.enqueue(&ctx(8)).await.unwrap_err(),
            SubmitFailure::Refused {
                status: 503,
                code: "paused".into()
            }
        );
        assert!(job_id(&pool, "discord-interaction:8").await.is_none());
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs DATABASE_URL (make test-db)"]
    async fn pg_upload_unreachable_worker_is_resubmitted_then_abandoned(pool: PgPool) {
        let api = Arc::new(Api::default());
        api.script
            .lock()
            .unwrap()
            .push((500, json!({ "error": "internal" })));
        let (q, _dir) = queue(pool.clone(), api.clone()).await;
        assert_eq!(q.enqueue(&ctx(9)).await.unwrap(), Queued::WillRetry);
        assert_eq!(job_id(&pool, "discord-interaction:9").await, Some(None));
        // The background pass submits it (same idempotency key).
        assert_eq!(q.resubmit_pending().await.unwrap(), 1);
        assert!(
            job_id(&pool, "discord-interaction:9")
                .await
                .unwrap()
                .is_some()
        );
        let keys: Vec<Value> = api
            .seen
            .lock()
            .unwrap()
            .iter()
            .map(|(_, b)| b["idempotency_key"].clone())
            .collect();
        assert_eq!(
            keys,
            [
                json!("discord-interaction:9"),
                json!("discord-interaction:9")
            ]
        );

        // Another one that never gets through within 10 minutes: abandoned with a notice.
        api.script.lock().unwrap().push((500, json!({})));
        q.enqueue(&ctx(10)).await.unwrap();
        sqlx::query("UPDATE library_upload_requests SET created_at = now() - interval '11 minutes' WHERE idempotency_key = 'discord-interaction:10'")
            .execute(&pool)
            .await
            .unwrap();
        q.resubmit_pending().await.unwrap();
        let outcome: Value = sqlx::query_scalar(
            "SELECT outcome FROM discord_notifications WHERE request_key = 'discord-interaction:10'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(outcome["kind"], "submission_failed");
    }
}
