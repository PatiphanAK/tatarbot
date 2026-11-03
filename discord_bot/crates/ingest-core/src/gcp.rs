//! Small REST clients for Google Cloud, authenticated with the workload's own service account
//! through the metadata server (Cloud Run, GCE). No service-account keys, no HMAC.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine as _;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::model::ProcessError;
use crate::ports::{ArtifactStore, TaskQueue};

/// Default metadata server; `GCE_METADATA_HOST` (the variable Google's libraries honour)
/// overrides it, e.g. for tests.
pub fn metadata_base() -> String {
    std::env::var("GCE_METADATA_HOST")
        .map(|h| format!("http://{h}"))
        .unwrap_or_else(|_| "http://metadata.google.internal".into())
}

#[async_trait]
pub trait AccessTokens: Send + Sync {
    async fn access_token(&self) -> Result<String, String>;
}

/// OAuth access tokens for the default service account, cached until a minute before expiry.
pub struct MetadataTokens {
    client: reqwest::Client,
    base: String,
    cache: Mutex<Option<(String, Instant)>>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: u64,
}

impl MetadataTokens {
    pub fn new(base: impl Into<String>) -> Result<Self, reqwest::Error> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()?,
            base: base.into(),
            cache: Mutex::new(None),
        })
    }
}

#[async_trait]
impl AccessTokens for MetadataTokens {
    async fn access_token(&self) -> Result<String, String> {
        let mut cache = self.cache.lock().await;
        if let Some((token, until)) = cache.as_ref()
            && Instant::now() < *until
        {
            return Ok(token.clone());
        }
        let response: TokenResponse = self
            .client
            .get(format!(
                "{}/computeMetadata/v1/instance/service-accounts/default/token",
                self.base
            ))
            .header("Metadata-Flavor", "Google")
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| format!("metadata token: {e}"))?
            .json()
            .await
            .map_err(|e| format!("metadata token: {e}"))?;
        let lifetime = Duration::from_secs(response.expires_in.saturating_sub(60).max(1));
        *cache = Some((response.access_token.clone(), Instant::now() + lifetime));
        Ok(response.access_token)
    }
}

// ---- Cloud Tasks

#[derive(Debug, Clone)]
pub struct CloudTasksConfig {
    /// `projects/<p>/locations/<l>/queues/<q>`
    pub queue: String,
    /// `<service URL>/internal/process`
    pub target_url: String,
    /// The OIDC audience: the service URL.
    pub audience: String,
    /// Service account whose OIDC token Cloud Tasks attaches (the caller needs actAs on it).
    pub invoker_service_account: String,
    pub dispatch_deadline_secs: u32,
    /// `https://cloudtasks.googleapis.com` (tests point it elsewhere).
    pub api_base: String,
}

pub struct CloudTasksQueue {
    client: reqwest::Client,
    tokens: Arc<dyn AccessTokens>,
    config: CloudTasksConfig,
}

impl CloudTasksQueue {
    pub fn new(
        config: CloudTasksConfig,
        tokens: Arc<dyn AccessTokens>,
    ) -> Result<Self, reqwest::Error> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()?,
            tokens,
            config,
        })
    }

    pub fn task_name(&self, job: Uuid, generation: i32) -> String {
        format!(
            "{}/tasks/job-{}-g{}",
            self.config.queue,
            job.simple(),
            generation
        )
    }
}

#[async_trait]
impl TaskQueue for CloudTasksQueue {
    async fn enqueue(&self, job: Uuid, generation: i32) -> Result<(), String> {
        let token = self.tokens.access_token().await?;
        let body =
            base64::engine::general_purpose::STANDARD.encode(json!({ "job_id": job }).to_string());
        let task = json!({
            "task": {
                "name": self.task_name(job, generation),
                "dispatchDeadline": format!("{}s", self.config.dispatch_deadline_secs),
                "httpRequest": {
                    "url": self.config.target_url,
                    "httpMethod": "POST",
                    "headers": { "Content-Type": "application/json" },
                    "body": body,
                    "oidcToken": {
                        "serviceAccountEmail": self.config.invoker_service_account,
                        "audience": self.config.audience,
                    },
                },
            },
        });
        let response = self
            .client
            .post(format!(
                "{}/v2/{}/tasks",
                self.config.api_base, self.config.queue
            ))
            .bearer_auth(token)
            .json(&task)
            .send()
            .await
            .map_err(|e| format!("cloud tasks: {e}"))?;
        match response.status().as_u16() {
            200..=299 => Ok(()),
            // ALREADY_EXISTS: this generation's task was created before (idempotent).
            409 => Ok(()),
            status => Err(format!(
                "cloud tasks: HTTP {status}: {}",
                response
                    .text()
                    .await
                    .unwrap_or_default()
                    .chars()
                    .take(300)
                    .collect::<String>()
            )),
        }
    }
}

/// In-process queue for local development: deliveries go to a channel the worker drains.
pub struct InlineQueue(pub tokio::sync::mpsc::UnboundedSender<Uuid>);

#[async_trait]
impl TaskQueue for InlineQueue {
    async fn enqueue(&self, job: Uuid, _generation: i32) -> Result<(), String> {
        self.0
            .send(job)
            .map_err(|_| "inline queue closed".to_owned())
    }
}

// ---- Cloud Storage (JSON API)

pub struct GcsClient {
    client: reqwest::Client,
    tokens: Arc<dyn AccessTokens>,
    bucket: String,
    /// `https://storage.googleapis.com` (tests point it elsewhere).
    api_base: String,
}

#[derive(Deserialize)]
struct ObjectMeta {
    name: String,
    updated: DateTime<Utc>,
}

#[derive(Deserialize)]
struct ObjectList {
    #[serde(default)]
    items: Vec<ObjectMeta>,
    #[serde(rename = "nextPageToken")]
    next_page_token: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PutOutcome {
    Created,
    AlreadyExists,
}

impl GcsClient {
    pub fn new(
        bucket: impl Into<String>,
        tokens: Arc<dyn AccessTokens>,
        api_base: impl Into<String>,
    ) -> Result<Self, reqwest::Error> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(120))
                .build()?,
            tokens,
            bucket: bucket.into(),
            api_base: api_base.into(),
        })
    }

    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    fn object_url(&self, key: &str) -> String {
        format!(
            "{}/storage/v1/b/{}/o/{}",
            self.api_base,
            self.bucket,
            url::form_urlencoded::byte_serialize(key.as_bytes()).collect::<String>()
        )
    }

    /// Create-only upload (`ifGenerationMatch=0`): an existing object is left untouched.
    pub async fn put_if_absent(
        &self,
        key: &str,
        path: &Path,
        content_type: &str,
    ) -> Result<PutOutcome, String> {
        let token = self.tokens.access_token().await?;
        let bytes = tokio::fs::read(path).await.map_err(|e| e.to_string())?;
        let response = self
            .client
            .post(format!(
                "{}/upload/storage/v1/b/{}/o",
                self.api_base, self.bucket
            ))
            .query(&[
                ("uploadType", "media"),
                ("name", key),
                ("ifGenerationMatch", "0"),
            ])
            .bearer_auth(token)
            .header(reqwest::header::CONTENT_TYPE, content_type)
            .body(bytes)
            .send()
            .await
            .map_err(|e| format!("gcs put: {e}"))?;
        match response.status().as_u16() {
            200..=299 => Ok(PutOutcome::Created),
            412 => Ok(PutOutcome::AlreadyExists),
            status => Err(format!("gcs put: HTTP {status}")),
        }
    }

    /// Last update time, or `None` if the object doesn't exist.
    pub async fn updated(&self, key: &str) -> Result<Option<DateTime<Utc>>, String> {
        let token = self.tokens.access_token().await?;
        let response = self
            .client
            .get(self.object_url(key))
            .query(&[("fields", "name,updated")])
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| format!("gcs get: {e}"))?;
        match response.status().as_u16() {
            404 => Ok(None),
            200..=299 => Ok(Some(
                response
                    .json::<ObjectMeta>()
                    .await
                    .map_err(|e| e.to_string())?
                    .updated,
            )),
            status => Err(format!("gcs get: HTTP {status}")),
        }
    }

    pub async fn delete(&self, key: &str) -> Result<(), String> {
        let token = self.tokens.access_token().await?;
        let response = self
            .client
            .delete(self.object_url(key))
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| format!("gcs delete: {e}"))?;
        match response.status().as_u16() {
            200..=299 | 404 => Ok(()),
            status => Err(format!("gcs delete: HTTP {status}")),
        }
    }

    pub async fn list(&self, prefix: &str) -> Result<Vec<(String, DateTime<Utc>)>, String> {
        let mut out = Vec::new();
        let mut page: Option<String> = None;
        loop {
            let token = self.tokens.access_token().await?;
            let mut query = vec![
                ("prefix", prefix.to_owned()),
                ("fields", "items(name,updated),nextPageToken".to_owned()),
            ];
            if let Some(p) = &page {
                query.push(("pageToken", p.clone()));
            }
            let list: ObjectList = self
                .client
                .get(format!("{}/storage/v1/b/{}/o", self.api_base, self.bucket))
                .query(&query)
                .bearer_auth(token)
                .send()
                .await
                .and_then(|r| r.error_for_status())
                .map_err(|e| format!("gcs list: {e}"))?
                .json()
                .await
                .map_err(|e| format!("gcs list: {e}"))?;
            out.extend(list.items.into_iter().map(|o| (o.name, o.updated)));
            match list.next_page_token {
                Some(token) => page = Some(token),
                None => return Ok(out),
            }
        }
    }
}

#[async_trait]
impl ArtifactStore for GcsClient {
    async fn put_if_absent(
        &self,
        key: &str,
        path: &Path,
        media_type: &str,
    ) -> Result<String, ProcessError> {
        GcsClient::put_if_absent(self, key, path, media_type)
            .await
            .map_err(ProcessError::Transient)?;
        Ok(format!("gs://{}/{key}", self.bucket))
    }
}

/// A local directory as an artifact store (development and tests).
pub struct LocalArtifacts(pub std::path::PathBuf);

#[async_trait]
impl ArtifactStore for LocalArtifacts {
    async fn put_if_absent(
        &self,
        key: &str,
        path: &Path,
        _media_type: &str,
    ) -> Result<String, ProcessError> {
        let dest = self.0.join(key);
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| ProcessError::Transient(e.to_string()))?;
        }
        if !tokio::fs::try_exists(&dest).await.unwrap_or(false) {
            tokio::fs::copy(path, &dest)
                .await
                .map_err(|e| ProcessError::Transient(e.to_string()))?;
        }
        Ok(format!("file://{}", dest.display()))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;

    use axum::extract::{Query, State};
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::{get, post};
    use axum::{Json, Router};

    use super::*;

    struct FixedToken;

    #[async_trait]
    impl AccessTokens for FixedToken {
        async fn access_token(&self) -> Result<String, String> {
            Ok("tok".into())
        }
    }

    async fn serve(router: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        format!("http://{addr}")
    }

    type Seen = Arc<StdMutex<Vec<(String, serde_json::Value)>>>;

    #[tokio::test]
    async fn cloud_tasks_creates_named_oidc_tasks_and_treats_409_as_done() {
        let seen: Seen = Arc::default();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let router = Router::new()
            .route(
                "/v2/projects/p/locations/l/queues/q/tasks",
                post(
                    |State((seen, calls)): State<(Seen, Arc<std::sync::atomic::AtomicUsize>)>,
                     headers: HeaderMap,
                     Json(body): Json<serde_json::Value>| async move {
                        let auth = headers["authorization"].to_str().unwrap().to_owned();
                        seen.lock().unwrap().push((auth, body));
                        if calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                            StatusCode::OK
                        } else {
                            StatusCode::CONFLICT
                        }
                    },
                ),
            )
            .with_state((seen.clone(), calls));
        let base = serve(router).await;
        let queue = CloudTasksQueue::new(
            CloudTasksConfig {
                queue: "projects/p/locations/l/queues/q".into(),
                target_url: "https://svc.run.app/internal/process".into(),
                audience: "https://svc.run.app".into(),
                invoker_service_account: "inv@p.iam.gserviceaccount.com".into(),
                dispatch_deadline_secs: 610,
                api_base: base,
            },
            Arc::new(FixedToken),
        )
        .unwrap();
        let job = Uuid::new_v4();
        queue.enqueue(job, 0).await.unwrap();
        queue.enqueue(job, 0).await.unwrap(); // 409 = already created
        let seen = seen.lock().unwrap();
        let (auth, body) = &seen[0];
        assert_eq!(auth, "Bearer tok");
        let task = &body["task"];
        assert_eq!(
            task["name"],
            format!(
                "projects/p/locations/l/queues/q/tasks/job-{}-g0",
                job.simple()
            )
        );
        assert_eq!(task["dispatchDeadline"], "610s");
        assert_eq!(
            task["httpRequest"]["oidcToken"]["audience"],
            "https://svc.run.app"
        );
        assert_eq!(
            task["httpRequest"]["oidcToken"]["serviceAccountEmail"],
            "inv@p.iam.gserviceaccount.com"
        );
        let payload = base64::engine::general_purpose::STANDARD
            .decode(task["httpRequest"]["body"].as_str().unwrap())
            .unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(payload, json!({ "job_id": job }));
    }

    #[tokio::test]
    async fn gcs_put_is_create_only() {
        let router = Router::new().route(
            "/upload/storage/v1/b/bkt/o",
            post(
                |Query(q): Query<std::collections::HashMap<String, String>>| async move {
                    assert_eq!(q["ifGenerationMatch"], "0");
                    assert_eq!(q["uploadType"], "media");
                    if q["name"] == "exists.ogg" {
                        StatusCode::PRECONDITION_FAILED
                    } else {
                        StatusCode::OK
                    }
                },
            ),
        );
        let base = serve(router).await;
        let gcs = GcsClient::new("bkt", Arc::new(FixedToken), base).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        tokio::fs::write(&file, b"x").await.unwrap();
        assert_eq!(
            gcs.put_if_absent("new.ogg", &file, "audio/ogg")
                .await
                .unwrap(),
            PutOutcome::Created
        );
        assert_eq!(
            gcs.put_if_absent("exists.ogg", &file, "audio/ogg")
                .await
                .unwrap(),
            PutOutcome::AlreadyExists
        );
    }

    #[tokio::test]
    async fn metadata_tokens_are_cached() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let router = Router::new()
            .route(
                "/computeMetadata/v1/instance/service-accounts/default/token",
                get(|State(calls): State<Arc<std::sync::atomic::AtomicUsize>>, headers: HeaderMap| async move {
                    assert_eq!(headers["metadata-flavor"], "Google");
                    calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Json(json!({ "access_token": "a1", "expires_in": 3599, "token_type": "Bearer" }))
                }),
            )
            .with_state(calls.clone());
        let base = serve(router).await;
        let tokens = MetadataTokens::new(base).unwrap();
        assert_eq!(tokens.access_token().await.unwrap(), "a1");
        assert_eq!(tokens.access_token().await.unwrap(), "a1");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
