//! Downloads an `https_url` source under the producer's policy: HTTPS only, exact host
//! allowlist (re-checked on every redirect), size limit enforced while streaming, SHA-256.

use std::path::Path;
use std::time::Duration;

use futures::StreamExt;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use url::Url;

use crate::model::{ProcessError, Producer, codes, hex};

const MAX_REDIRECTS: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetched {
    pub size: u64,
    pub sha256: String,
}

#[derive(Clone)]
pub struct HttpsFetcher {
    client: reqwest::Client,
    /// Tests only: also accept `http://127.0.0.1:<port>` (still host-checked).
    allow_loopback_http: bool,
}

impl HttpsFetcher {
    pub fn new(timeout: Duration) -> Result<Self, reqwest::Error> {
        Ok(Self {
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(timeout)
                .connect_timeout(Duration::from_secs(10))
                .build()?,
            allow_loopback_http: false,
        })
    }

    #[doc(hidden)]
    pub fn allowing_loopback_http(mut self) -> Self {
        self.allow_loopback_http = true;
        self
    }

    /// Whether `url` may be fetched for `producer` (used at submission time, too).
    pub fn check_url(&self, producer: &Producer, url: &str) -> Result<Url, ProcessError> {
        check_url(producer, url, self.allow_loopback_http)
    }

    pub async fn fetch(
        &self,
        producer: &Producer,
        url: &str,
        dest: &Path,
    ) -> Result<Fetched, ProcessError> {
        let mut url = self.check_url(producer, url)?;
        let max = producer.max_source_bytes;
        for _ in 0..=MAX_REDIRECTS {
            let response = self
                .client
                .get(url.clone())
                .send()
                .await
                .map_err(|e| ProcessError::Transient(format!("fetch: {e}")))?;
            let status = response.status();
            if status.is_redirection() {
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| ProcessError::permanent(codes::SOURCE_GONE, None))?;
                let next = url
                    .join(location)
                    .map_err(|_| ProcessError::permanent(codes::SOURCE_GONE, None))?;
                url = self.check_url(producer, next.as_str())?;
                continue;
            }
            if status.is_server_error() || status.as_u16() == 429 {
                return Err(ProcessError::Transient(format!("fetch: HTTP {status}")));
            }
            if !status.is_success() {
                // 403/404/410: the URL expired or the file is gone; retrying can't help.
                return Err(ProcessError::permanent(codes::SOURCE_GONE, None));
            }
            if response.content_length().is_some_and(|len| len > max) {
                return Err(too_large(max));
            }
            return stream_to(response, dest, max).await;
        }
        Err(ProcessError::permanent(codes::SOURCE_GONE, None))
    }
}

fn too_large(max: u64) -> ProcessError {
    ProcessError::Permanent {
        code: codes::TOO_LARGE.into(),
        detail: None,
        data: serde_json::json!({ "max_bytes": max }),
    }
}

async fn stream_to(
    response: reqwest::Response,
    dest: &Path,
    max: u64,
) -> Result<Fetched, ProcessError> {
    let io = |e: std::io::Error| ProcessError::Transient(format!("write source: {e}"));
    let mut file = tokio::fs::File::create(dest).await.map_err(io)?;
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut body = response.bytes_stream();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|e| ProcessError::Transient(format!("fetch body: {e}")))?;
        size += chunk.len() as u64;
        if size > max {
            return Err(too_large(max));
        }
        hasher.update(&chunk);
        file.write_all(&chunk).await.map_err(io)?;
    }
    file.flush().await.map_err(io)?;
    Ok(Fetched {
        size,
        sha256: hex(&hasher.finalize()),
    })
}

fn check_url(
    producer: &Producer,
    raw: &str,
    allow_loopback_http: bool,
) -> Result<Url, ProcessError> {
    let denied =
        || ProcessError::permanent(codes::POLICY_DENIED, Some("source URL not allowed".into()));
    let url = Url::parse(raw).map_err(|_| denied())?;
    let host = url.host_str().ok_or_else(denied)?;
    let scheme_ok = url.scheme() == "https"
        || (allow_loopback_http && url.scheme() == "http" && host == "127.0.0.1");
    if !scheme_ok || !url.username().is_empty() || url.password().is_some() {
        return Err(denied());
    }
    if !producer.source_url_hosts.iter().any(|h| h == host) {
        return Err(denied());
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn producer(hosts: &[&str], max: u64) -> Producer {
        Producer {
            id: "p".into(),
            service_account: "p@x".into(),
            tenant_prefix: "t:".into(),
            allowed_pipelines: vec![],
            allowed_source_kinds: vec!["https_url".into()],
            source_url_hosts: hosts.iter().map(|h| h.to_string()).collect(),
            max_source_bytes: max,
            max_active_per_actor: 3,
            max_jobs_per_tenant_hour: 20,
            enabled: true,
        }
    }

    #[test]
    fn only_https_to_listed_hosts() {
        let p = producer(&["cdn.example.com"], 10);
        assert!(check_url(&p, "https://cdn.example.com/a.mp3?x=1", false).is_ok());
        for bad in [
            "http://cdn.example.com/a.mp3",
            "https://cdn.example.com.evil.test/a.mp3",
            "https://evil.test/a.mp3",
            "https://user:pw@cdn.example.com/a.mp3",
            "file:///etc/passwd",
            "not a url",
            "http://127.0.0.1:9/a.mp3",
        ] {
            assert!(check_url(&p, bad, false).is_err(), "{bad}");
        }
    }

    async fn serve(router: axum::Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn streams_hashes_and_enforces_the_limit() {
        use axum::routing::get;
        let base = serve(
            axum::Router::new()
                .route("/small", get(|| async { "hello" }))
                .route("/big", get(|| async { "x".repeat(100) }))
                .route("/gone", get(|| async { axum::http::StatusCode::NOT_FOUND }))
                .route(
                    "/busy",
                    get(|| async { axum::http::StatusCode::SERVICE_UNAVAILABLE }),
                )
                .route(
                    "/redirect-out",
                    get(|| async { axum::response::Redirect::temporary("https://evil.test/x") }),
                ),
        )
        .await;
        let p = producer(&["127.0.0.1"], 10);
        let f = HttpsFetcher::new(Duration::from_secs(5))
            .unwrap()
            .allowing_loopback_http();
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("in");

        let ok = f.fetch(&p, &format!("{base}/small"), &dest).await.unwrap();
        assert_eq!(ok.size, 5);
        assert_eq!(
            ok.sha256,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        let err = f
            .fetch(&p, &format!("{base}/big"), &dest)
            .await
            .unwrap_err();
        assert!(matches!(err, ProcessError::Permanent { ref code, .. } if code == "too_large"));
        let err = f
            .fetch(&p, &format!("{base}/gone"), &dest)
            .await
            .unwrap_err();
        assert!(matches!(err, ProcessError::Permanent { ref code, .. } if code == "source_gone"));
        let err = f
            .fetch(&p, &format!("{base}/busy"), &dest)
            .await
            .unwrap_err();
        assert!(matches!(err, ProcessError::Transient(_)));
        let err = f
            .fetch(&p, &format!("{base}/redirect-out"), &dest)
            .await
            .unwrap_err();
        assert!(matches!(err, ProcessError::Permanent { ref code, .. } if code == "policy_denied"));
    }
}
