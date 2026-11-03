//! The ingest worker (ADR 0004): `ingest-core` + the `audio.opus-v1` pipeline + the TatarBot
//! library consumer, served on Cloud Run.
//!
//! ```text
//! ingest-worker [serve]                      HTTP API on $PORT (Cloud Run)
//! ingest-worker migrate                      apply the `ingest` schema migrations
//! ingest-worker register-producer --id ID --service-account EMAIL --tenant-prefix P
//!     --pipelines A,B --source-kinds https_url --hosts H1,H2 --max-bytes N
//!     [--max-active N] [--per-hour N] [--disabled]
//! ingest-worker switch pause|resume SCOPE REASON   kill switch (global, producer:ID, pipeline:ID)
//! ingest-worker bench FILE...                run the pipeline on files, print JSON metrics
//! ```
//!
//! Environment (serve): DATABASE_URL, DATABASE_POOL_SIZE (4), PORT (8080), INGEST_PUBLIC_URL
//! (this service's URL), INGEST_AUDIENCES (accepted token audiences, default the public URL),
//! INGEST_INTERNAL_CALLERS (service accounts allowed on /internal/*), INGEST_QUEUE
//! (projects/P/locations/L/queues/Q, or `inline` for local development),
//! INGEST_TASKS_INVOKER_SA, MUSIC_BUCKET, MUSIC_PREFIX (library/), MUSIC_GUILD_QUOTA_MB (500),
//! FFMPEG_PATH, FFPROBE_PATH, INGEST_JWKS_URL (Google's by default).

use std::env;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ingest_core::auth::{GOOGLE_JWKS_URL, GoogleKeys, IdTokenVerifier};
use ingest_core::fetch::HttpsFetcher;
use ingest_core::gcp::{
    CloudTasksConfig, CloudTasksQueue, GcsClient, InlineQueue, MetadataTokens, metadata_base,
};
use ingest_core::http::{ApiState, router};
use ingest_core::{ArtifactStore, Engine, Pipeline, Producer, Repo, TaskQueue};
use serde_json::json;
use sqlx::postgres::PgPoolOptions;
use tatarbot_discord::adapters::outbound::ffmpeg::{Ffmpeg, FfmpegSettings};
use tatarbot_discord::adapters::outbound::pg_library::PgLibrary;
use tatarbot_discord::ingest::consumer::{LibraryConsumer, PRODUCER_ID};
use tatarbot_discord::ingest::gcs::GcsObjectStore;
use tatarbot_discord::ingest::pipeline::{AudioOpusPipeline, PIPELINE_ID};
use tatarbot_discord::tls;
use tracing_subscriber::EnvFilter;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    if let Err(err) = tls::install_crypto_provider() {
        eprintln!("TLS setup failed: {err}");
        std::process::exit(1);
    }
    let args: Vec<String> = env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        None | Some("serve") => serve().await,
        Some("migrate") => migrate().await,
        Some("register-producer") => register_producer(&args[1..]).await,
        Some("switch") => switch(&args[1..]).await,
        Some("bench") => bench(&args[1..]).await,
        Some(other) => Err(format!("unknown command {other}").into()),
    };
    if let Err(err) = result {
        tracing::error!(%err, "ingest-worker failed");
        std::process::exit(1);
    }
}

fn required(key: &str) -> Result<String, BoxError> {
    env::var(key)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| format!("missing environment variable {key}").into())
}

fn list(key: &str) -> Vec<String> {
    env::var(key)
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect()
}

fn number<T: std::str::FromStr>(key: &str, default: T) -> Result<T, BoxError> {
    match env::var(key) {
        Ok(v) if !v.trim().is_empty() => v
            .trim()
            .parse()
            .map_err(|_| format!("{key} must be a number").into()),
        _ => Ok(default),
    }
}

async fn pool() -> Result<sqlx::PgPool, BoxError> {
    Ok(PgPoolOptions::new()
        .max_connections(number("DATABASE_POOL_SIZE", 4u32)?)
        .acquire_timeout(Duration::from_secs(10))
        .connect_lazy(&required("DATABASE_URL")?)?)
}

fn ffmpeg() -> Ffmpeg {
    // Empty means unset; bare names are resolved against PATH by `Ffmpeg::new`.
    let path = |key: &str| env::var(key).ok().filter(|v| !v.trim().is_empty());
    let mut settings = FfmpegSettings::default();
    if let Some(p) = path("FFMPEG_PATH") {
        settings.ffmpeg = p.into();
    }
    if let Some(p) = path("FFPROBE_PATH") {
        settings.ffprobe = p.into();
    }
    Ffmpeg::new(settings)
}

async fn serve() -> Result<(), BoxError> {
    let pool = pool().await?;
    let migrated: bool = sqlx::query_scalar("SELECT to_regclass('ingest.jobs') IS NOT NULL")
        .fetch_one(&pool)
        .await?;
    if !migrated {
        return Err("the ingest schema is missing: run `ingest-worker migrate` first".into());
    }
    let public_url = required("INGEST_PUBLIC_URL")?
        .trim_end_matches('/')
        .to_owned();
    let mut audiences = list("INGEST_AUDIENCES");
    if audiences.is_empty() {
        audiences.push(public_url.clone());
    }
    let internal_callers = list("INGEST_INTERNAL_CALLERS");
    if internal_callers.is_empty() {
        return Err("INGEST_INTERNAL_CALLERS is empty: nothing could deliver tasks".into());
    }

    let tokens = Arc::new(MetadataTokens::new(metadata_base())?);
    let gcs = Arc::new(GcsClient::new(
        required("MUSIC_BUCKET")?,
        tokens.clone(),
        "https://storage.googleapis.com",
    )?);
    let prefix = env::var("MUSIC_PREFIX").unwrap_or_else(|_| "library/".into());
    let quota_mb: u64 = number("MUSIC_GUILD_QUOTA_MB", 500)?;

    let (inline_tx, inline_rx) = tokio::sync::mpsc::unbounded_channel();
    let queue_name = required("INGEST_QUEUE")?;
    let queue: Arc<dyn TaskQueue> = if queue_name == "inline" {
        tracing::warn!("INGEST_QUEUE=inline: in-process delivery, for local development only");
        Arc::new(InlineQueue(inline_tx))
    } else {
        Arc::new(CloudTasksQueue::new(
            CloudTasksConfig {
                queue: queue_name,
                target_url: format!("{public_url}/internal/process"),
                audience: public_url.clone(),
                invoker_service_account: required("INGEST_TASKS_INVOKER_SA")?,
                dispatch_deadline_secs: 610,
                api_base: "https://cloudtasks.googleapis.com".into(),
            },
            tokens.clone(),
        )?)
    };

    let consumer = LibraryConsumer::new(
        PgLibrary::from_pool(pool.clone(), true),
        Arc::new(GcsObjectStore::new(gcs.clone())),
        prefix,
        quota_mb * 1024 * 1024,
    );
    let artifacts: Arc<dyn ArtifactStore> = gcs;
    let engine = Arc::new(
        Engine::builder(
            Repo::new(pool),
            artifacts,
            queue,
            HttpsFetcher::new(Duration::from_secs(60))?,
        )
        .pipeline(PIPELINE_ID, Arc::new(AudioOpusPipeline::new(ffmpeg())))
        .consumer(PRODUCER_ID, Arc::new(consumer))
        .build(),
    );
    spawn_inline_delivery(engine.clone(), inline_rx);

    let jwks = env::var("INGEST_JWKS_URL").unwrap_or_else(|_| GOOGLE_JWKS_URL.into());
    let state = Arc::new(ApiState {
        engine,
        verifier: Arc::new(IdTokenVerifier::google(
            Arc::new(GoogleKeys::new(jwks)?),
            audiences,
        )),
        internal_callers,
    });
    let port: u16 = number("PORT", 8080)?;
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    tracing::info!(port, "ingest worker listening");
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// SIGTERM (Cloud Run) or Ctrl-C: stop accepting, finish in-flight requests.
async fn shutdown_signal() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("SIGTERM handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
    tracing::info!("shutting down");
}

/// `INGEST_QUEUE=inline`: deliveries from the in-process queue, retried a few times.
fn spawn_inline_delivery(
    engine: Arc<Engine>,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<uuid::Uuid>,
) {
    tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            for attempt in 0..4u32 {
                match engine.process(job).await {
                    Ok(ingest_core::Delivery::Done) => break,
                    Ok(ingest_core::Delivery::Retry) | Err(_) => {
                        tokio::time::sleep(Duration::from_secs(2u64.pow(attempt))).await;
                    }
                }
            }
        }
    });
}

async fn migrate() -> Result<(), BoxError> {
    let pool = pool().await?;
    let applied = ingest_core::migrate::migrate(&pool).await?;
    tracing::info!(?applied, "ingest migrations applied");
    Ok(())
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn csv(value: Option<String>) -> Vec<String> {
    value
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect()
}

async fn register_producer(args: &[String]) -> Result<(), BoxError> {
    let need = |name: &str| flag(args, name).ok_or_else(|| format!("missing {name}"));
    let producer = Producer {
        id: need("--id")?,
        service_account: need("--service-account")?,
        tenant_prefix: need("--tenant-prefix")?,
        allowed_pipelines: csv(Some(need("--pipelines")?)),
        allowed_source_kinds: csv(Some(need("--source-kinds")?)),
        source_url_hosts: csv(flag(args, "--hosts")),
        max_source_bytes: need("--max-bytes")?.parse()?,
        max_active_per_actor: flag(args, "--max-active").map_or(Ok(3), |v| v.parse())?,
        max_jobs_per_tenant_hour: flag(args, "--per-hour").map_or(Ok(20), |v| v.parse())?,
        enabled: !args.iter().any(|a| a == "--disabled"),
    };
    Repo::new(pool().await?).upsert_producer(&producer).await?;
    tracing::info!(
        id = producer.id,
        account = producer.service_account,
        "producer registered"
    );
    Ok(())
}

async fn switch(args: &[String]) -> Result<(), BoxError> {
    let (Some(action), Some(scope)) = (args.first(), args.get(1)) else {
        return Err("usage: switch pause|resume SCOPE REASON".into());
    };
    let paused = match action.as_str() {
        "pause" => true,
        "resume" => false,
        _ => return Err("usage: switch pause|resume SCOPE REASON".into()),
    };
    let reason = args[2..].join(" ");
    let reason = if reason.is_empty() {
        action.clone()
    } else {
        reason
    };
    let by = format!(
        "operator:{}",
        env::var("USER").unwrap_or_else(|_| "unknown".into())
    );
    Repo::new(pool().await?)
        .set_switch(scope, paused, &reason, &by)
        .await?;
    tracing::info!(scope, paused, reason, "switch updated");
    Ok(())
}

fn child_cpu_and_rss() -> (Duration, i64) {
    // SAFETY: getrusage writes into the zeroed struct we own.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &mut usage) };
    let tv = |t: libc::timeval| Duration::new(t.tv_sec as u64, t.tv_usec as u32 * 1000);
    (tv(usage.ru_utime) + tv(usage.ru_stime), usage.ru_maxrss)
}

/// ADR 0004 benchmark plan: one JSON line per file.
async fn bench(files: &[String]) -> Result<(), BoxError> {
    if files.is_empty() {
        return Err("usage: bench FILE...".into());
    }
    let pipeline = AudioOpusPipeline::new(ffmpeg());
    for file in files {
        let out = tempfile::tempdir()?;
        let (cpu_before, _) = child_cpu_and_rss();
        let started = Instant::now();
        let result = pipeline
            .run(std::path::Path::new(file), out.path(), &json!({}))
            .await;
        let wall = started.elapsed();
        let (cpu_after, max_rss_kb) = child_cpu_and_rss();
        let line = match result {
            Ok(output) => json!({
                "file": file, "ok": true,
                "wall_ms": wall.as_millis() as u64,
                "child_cpu_ms": (cpu_after - cpu_before).as_millis() as u64,
                "max_child_rss_kb": max_rss_kb,
                "input_bytes": std::fs::metadata(file).map(|m| m.len()).unwrap_or(0),
                "output_bytes": std::fs::metadata(&output.path).map(|m| m.len()).unwrap_or(0),
                "duration_ms": output.metadata["duration_ms"],
            }),
            Err(err) => json!({
                "file": file, "ok": false, "error": err.to_string(),
                "wall_ms": wall.as_millis() as u64,
            }),
        };
        println!("{line}");
    }
    Ok(())
}
