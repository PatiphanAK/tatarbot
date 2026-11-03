//! The HTTP API's caller identity, through the real router and token verifier (test keys).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine as _;
use ingest_core::auth::{IdTokenVerifier, StaticKeys, testing};
use ingest_core::fetch::HttpsFetcher;
use ingest_core::gcp::LocalArtifacts;
use ingest_core::http::{ApiState, router};
use ingest_core::{Engine, Pipeline, PipelineOutput, ProcessError, Producer, Repo, TaskQueue};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

const AUD: &str = "https://tatarbot-ingest-123.asia-southeast1.run.app";
const PRODUCER_SA: &str = "tatarbot-vm@p.iam.gserviceaccount.com";
const INVOKER_SA: &str = "tatarbot-tasks-invoker@p.iam.gserviceaccount.com";

struct NoQueue;

#[async_trait]
impl TaskQueue for NoQueue {
    async fn enqueue(&self, _: Uuid, _: i32) -> Result<(), String> {
        Ok(())
    }
}

struct NoopPipeline;

#[async_trait]
impl Pipeline for NoopPipeline {
    fn version(&self) -> &str {
        "noop"
    }
    fn validate_params(&self, _: &Value) -> Result<(), String> {
        Ok(())
    }
    async fn run(
        &self,
        input: &std::path::Path,
        _: &std::path::Path,
        _: &Value,
    ) -> Result<PipelineOutput, ProcessError> {
        Ok(PipelineOutput {
            path: input.to_owned(),
            media_type: "x".into(),
            extension: "bin".into(),
            metadata: json!({}),
        })
    }
}

async fn app(pool: PgPool) -> (axum::Router, tempfile::TempDir) {
    ingest_core::migrate::migrate(&pool).await.unwrap();
    let repo = Repo::new(pool);
    repo.upsert_producer(&Producer {
        id: "tatarbot".into(),
        service_account: PRODUCER_SA.into(),
        tenant_prefix: "discord:".into(),
        allowed_pipelines: vec!["noop.v1".into()],
        allowed_source_kinds: vec!["https_url".into()],
        source_url_hosts: vec!["cdn.discordapp.com".into()],
        max_source_bytes: 26_214_400,
        max_active_per_actor: 3,
        max_jobs_per_tenant_hour: 20,
        enabled: true,
    })
    .await
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::builder(
        repo,
        Arc::new(LocalArtifacts(dir.path().to_owned())),
        Arc::new(NoQueue),
        HttpsFetcher::new(Duration::from_secs(5)).unwrap(),
    )
    .pipeline("noop.v1", Arc::new(NoopPipeline))
    .build();
    let state = Arc::new(ApiState {
        engine: Arc::new(engine),
        verifier: Arc::new(IdTokenVerifier::google(
            Arc::new(StaticKeys(testing::jwks())),
            vec![AUD.into()],
        )),
        internal_callers: vec![INVOKER_SA.into()],
    });
    (router(state), dir)
}

fn submit_body() -> Value {
    json!({
        "idempotency_key": "discord-interaction:1",
        "tenant": "discord:guild:1",
        "actor": "discord:user:2",
        "pipeline": "noop.v1",
        "params": {},
        "source": { "kind": "https_url", "uri": "https://cdn.discordapp.com/a/b/song.mp3", "declared_size": 1000 }
    })
}

async fn call(
    app: &axum::Router,
    path: &str,
    auth: Option<String>,
    body: Value,
) -> (StatusCode, Value) {
    let mut req = Request::post(path).header("content-type", "application/json");
    if let Some(a) = auth {
        req = req.header("authorization", a);
    }
    let response = app
        .clone()
        .oneshot(req.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn bearer(email: &str) -> Option<String> {
    Some(format!("Bearer {}", testing::token(email, AUD, true, 300)))
}

#[sqlx::test(migrations = false)]
#[ignore = "needs DATABASE_URL (make test-db)"]
async fn pg_producer_routes_need_a_registered_producer_token(pool: PgPool) {
    let (app, _dir) = app(pool).await;
    let (status, body) = call(&app, "/v1/jobs", bearer(PRODUCER_SA), submit_body()).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["state"], "queued");
    // Same key again: the same job, 200.
    let (status, again) = call(&app, "/v1/jobs", bearer(PRODUCER_SA), submit_body()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again["job_id"], body["job_id"]);

    // The tasks invoker is not a producer; no token / wrong audience / stripped signature fail.
    let (status, _) = call(&app, "/v1/jobs", bearer(INVOKER_SA), submit_body()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(&app, "/v1/jobs", None, submit_body()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // Authentication comes before body validation.
    let (status, _) = call(&app, "/v1/jobs", None, json!({ "garbage": true })).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = call(
        &app,
        "/v1/jobs",
        bearer(PRODUCER_SA),
        json!({ "garbage": true }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let other_aud = Some(format!(
        "Bearer {}",
        testing::token(PRODUCER_SA, "https://other.run.app", true, 300)
    ));
    let (status, _) = call(&app, "/v1/jobs", other_aud, submit_body()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let token = testing::token(PRODUCER_SA, AUD, true, 300);
    let stripped = Some(format!("Bearer {}.", token.rsplit_once('.').unwrap().0));
    let (status, _) = call(&app, "/v1/jobs", stripped, submit_body()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Policy errors are 400 with a code.
    let mut bad = submit_body();
    bad["source"]["uri"] = "https://evil.test/a.mp3".into();
    bad["idempotency_key"] = "k2".into();
    let (status, body) = call(&app, "/v1/jobs", bearer(PRODUCER_SA), bad).await;
    assert_eq!(
        (status, body["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("policy_denied"))
    );
}

#[sqlx::test(migrations = false)]
#[ignore = "needs DATABASE_URL (make test-db)"]
async fn pg_internal_routes_accept_only_the_tasks_invoker(pool: PgPool) {
    let (app, _dir) = app(pool).await;
    let body = json!({ "job_id": Uuid::new_v4() });
    let (status, _) = call(&app, "/internal/process", bearer(PRODUCER_SA), body.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(&app, "/internal/process", None, body.clone()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // Unknown job: acknowledged so Cloud Tasks stops redelivering.
    let (status, _) = call(&app, "/internal/process", bearer(INVOKER_SA), body).await;
    assert_eq!(status, StatusCode::OK);

    let (status, report) = call(
        &app,
        "/internal/maintenance/reap",
        bearer(INVOKER_SA),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(report["enqueued"], 0);
    let (status, _) = call(
        &app,
        "/internal/maintenance/reap",
        bearer(PRODUCER_SA),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let data = base64::engine::general_purpose::STANDARD
        .encode(r#"{"costAmount": 11.0, "budgetAmount": 10.0}"#);
    let push = json!({ "message": { "data": data } });
    let (status, _) = call(
        &app,
        "/internal/maintenance/budget",
        bearer(INVOKER_SA),
        push,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    // Now paused: submissions get 503.
    let (status, body) = call(&app, "/v1/jobs", bearer(PRODUCER_SA), submit_body()).await;
    assert_eq!(
        (status, body["error"].as_str()),
        (StatusCode::SERVICE_UNAVAILABLE, Some("paused"))
    );
}
