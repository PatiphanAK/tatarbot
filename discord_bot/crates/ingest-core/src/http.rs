//! The worker's HTTP API (ADR 0004). Every route verifies the caller's Google ID token in
//! the app; Cloud Run IAM in front only decides who may reach the service at all.
//!
//! * `/v1/jobs*`: producers, identified by the token's `email` (`ingest.producers`).
//! * `/internal/*`: Cloud Tasks, Cloud Scheduler and the budget push subscription, which all
//!   sign as the tasks-invoker service account(s) in `internal_callers`.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::auth::{AuthError, Caller, IdTokenVerifier};
use crate::engine::{Delivery, Engine, SubmitError};
use crate::model::{JobView, Producer, SubmitRequest};

pub struct ApiState {
    pub engine: Arc<Engine>,
    pub verifier: Arc<IdTokenVerifier>,
    pub internal_callers: Vec<String>,
}

pub fn router(state: Arc<ApiState>) -> Router {
    Router::new()
        .route("/v1/jobs", post(submit))
        .route("/v1/jobs/{id}", get(get_job))
        .route("/v1/jobs/{id}/cancel", post(cancel))
        .route("/internal/process", post(process))
        .route("/internal/maintenance/reap", post(reap))
        .route("/internal/maintenance/consumers", post(maintain))
        .route("/internal/maintenance/budget", post(budget))
        .with_state(state)
}

struct ApiError(StatusCode, &'static str, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1, "detail": self.2 }))).into_response()
    }
}

fn internal(e: impl std::fmt::Display) -> ApiError {
    tracing::error!(error = %e, "internal error");
    ApiError(StatusCode::INTERNAL_SERVER_ERROR, "internal", String::new())
}

async fn caller(state: &ApiState, headers: &HeaderMap) -> Result<Caller, ApiError> {
    let header = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    state.verifier.verify(header).await.map_err(|e| match e {
        AuthError::Keys(e) => {
            tracing::error!(error = %e, "cannot load token signing keys");
            ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "keys_unavailable",
                String::new(),
            )
        }
        e => {
            tracing::info!(error = %e, "rejected caller");
            ApiError(StatusCode::UNAUTHORIZED, "unauthenticated", String::new())
        }
    })
}

async fn producer(state: &ApiState, headers: &HeaderMap) -> Result<Producer, ApiError> {
    let caller = caller(state, headers).await?;
    state
        .engine
        .repo()
        .producer_by_account(&caller.email)
        .await
        .map_err(internal)?
        .ok_or_else(|| {
            tracing::info!(email = %caller.email, "caller is not a registered producer");
            ApiError(StatusCode::FORBIDDEN, "not_a_producer", String::new())
        })
}

async fn internal_caller(state: &ApiState, headers: &HeaderMap) -> Result<(), ApiError> {
    let caller = caller(state, headers).await?;
    if state.internal_callers.iter().any(|c| c == &caller.email) {
        Ok(())
    } else {
        tracing::info!(email = %caller.email, "caller may not use internal routes");
        Err(ApiError(StatusCode::FORBIDDEN, "forbidden", String::new()))
    }
}

/// Bodies are parsed only after the caller is authenticated, so unauthenticated requests
/// learn nothing about validation.
fn parse<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, ApiError> {
    serde_json::from_slice(body)
        .map_err(|e| ApiError(StatusCode::BAD_REQUEST, "bad_request", e.to_string()))
}

async fn submit(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let producer = producer(&state, &headers).await?;
    let req: SubmitRequest = parse(&body)?;
    match state.engine.submit(&producer, &req).await {
        Ok((job, created)) => {
            let status = if created {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            };
            Ok((status, Json(JobView::from(&job))).into_response())
        }
        Err(SubmitError::ProducerDisabled) => Err(ApiError(
            StatusCode::FORBIDDEN,
            "producer_disabled",
            String::new(),
        )),
        Err(SubmitError::Policy(m)) => Err(ApiError(StatusCode::BAD_REQUEST, "policy_denied", m)),
        Err(e @ SubmitError::TooManyActive(_)) => Err(ApiError(
            StatusCode::TOO_MANY_REQUESTS,
            "too_many_active",
            e.to_string(),
        )),
        Err(e @ SubmitError::TenantRateLimited(_)) => Err(ApiError(
            StatusCode::TOO_MANY_REQUESTS,
            "tenant_rate_limited",
            e.to_string(),
        )),
        Err(SubmitError::Paused(m)) => Err(ApiError(StatusCode::SERVICE_UNAVAILABLE, "paused", m)),
        Err(SubmitError::Internal(m)) => Err(internal(m)),
    }
}

async fn get_job(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<JobView>, ApiError> {
    let producer = producer(&state, &headers).await?;
    match state.engine.repo().job(id).await.map_err(internal)? {
        Some(job) if job.producer_id == producer.id => Ok(Json(JobView::from(&job))),
        _ => Err(ApiError(StatusCode::NOT_FOUND, "not_found", String::new())),
    }
}

async fn cancel(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Response, ApiError> {
    let producer = producer(&state, &headers).await?;
    match state
        .engine
        .cancel(&producer.id, id)
        .await
        .map_err(internal)?
    {
        Some(job) => Ok((StatusCode::ACCEPTED, Json(JobView::from(&job))).into_response()),
        None => Err(ApiError(StatusCode::NOT_FOUND, "not_found", String::new())),
    }
}

#[derive(Deserialize)]
struct ProcessBody {
    job_id: Uuid,
}

async fn process(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    internal_caller(&state, &headers).await?;
    let body: ProcessBody = parse(&body)?;
    match state.engine.process(body.job_id).await {
        Ok(Delivery::Done) => Ok(StatusCode::OK),
        // Cloud Tasks retries any non-2xx with backoff.
        Ok(Delivery::Retry) => Ok(StatusCode::SERVICE_UNAVAILABLE),
        Err(e) => {
            tracing::warn!(job = %body.job_id, error = %e, "process failed; retrying");
            Ok(StatusCode::SERVICE_UNAVAILABLE)
        }
    }
}

async fn reap(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    internal_caller(&state, &headers).await?;
    let report = state.engine.reap().await.map_err(internal)?;
    Ok(Json(serde_json::to_value(report).unwrap_or_default()))
}

async fn maintain(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    internal_caller(&state, &headers).await?;
    Ok(Json(state.engine.maintain_consumers().await))
}

/// A Pub/Sub push of a Cloud Billing budget notification. Always acknowledged (2xx) unless
/// the database write fails, so malformed messages aren't redelivered forever.
async fn budget(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    internal_caller(&state, &headers).await?;
    let envelope: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let Some((cost, budget)) = parse_budget(&envelope) else {
        tracing::warn!("unreadable budget notification; acknowledged");
        return Ok(StatusCode::NO_CONTENT);
    };
    state
        .engine
        .budget_update(cost, budget)
        .await
        .map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

fn parse_budget(envelope: &Value) -> Option<(f64, f64)> {
    let data = envelope.get("message")?.get("data")?.as_str()?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .ok()?;
    let body: Value = serde_json::from_slice(&bytes).ok()?;
    Some((
        body.get("costAmount")?.as_f64()?,
        body.get("budgetAmount")?.as_f64()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_budget_notifications() {
        let data = base64::engine::general_purpose::STANDARD
            .encode(r#"{"budgetDisplayName":"tatarbot","costAmount":12.5,"budgetAmount":10.0,"currencyCode":"USD"}"#);
        let envelope =
            json!({ "message": { "data": data, "messageId": "1" }, "subscription": "s" });
        assert_eq!(parse_budget(&envelope), Some((12.5, 10.0)));
        assert_eq!(parse_budget(&json!({ "message": {} })), None);
    }
}
