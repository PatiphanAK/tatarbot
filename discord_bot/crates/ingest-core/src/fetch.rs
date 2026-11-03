//! Downloads an `https_url` source under the producer's policy: HTTPS only, exact host
//! allowlist (re-checked on every redirect), size limit enforced while streaming, SHA-256.
//!
//! SSRF defence in depth: even an allowlisted name can't reach internal addresses. Every DNS
//! answer is filtered to public addresses at connect time (so DNS rebinding or a hijacked
//! record gains nothing), and IP-literal hosts must be public.

use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::sync::Arc;
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
        Self::build(timeout, false)
    }

    fn build(timeout: Duration, allow_loopback_http: bool) -> Result<Self, reqwest::Error> {
        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .connect_timeout(Duration::from_secs(10));
        if !allow_loopback_http {
            builder = builder.dns_resolver(Arc::new(PublicOnlyResolver));
        }
        Ok(Self {
            client: builder.build()?,
            allow_loopback_http,
        })
    }

    /// Tests only: accept `http://127.0.0.1:<port>` and skip the public-address filter.
    #[doc(hidden)]
    pub fn allowing_loopback_http(self) -> Self {
        let timeout = Duration::from_secs(30);
        Self::build(timeout, true).expect("client builds")
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
    // An allowlisted IP literal must still be public (a misconfigured policy can't open
    // the metadata server or the VPC).
    let literal = match url.host() {
        Some(url::Host::Ipv4(ip)) => Some(IpAddr::V4(ip)),
        Some(url::Host::Ipv6(ip)) => Some(IpAddr::V6(ip)),
        _ => None,
    };
    if let Some(ip) = literal
        && !is_public_ip(ip)
        && !(allow_loopback_http && ip.is_loopback())
    {
        return Err(denied());
    }
    Ok(url)
}

/// Public unicast addresses only (no loopback, private, link-local incl. 169.254.169.254,
/// CGNAT, unique-local, documentation, multicast, reserved; IPv4-mapped IPv6 unwrapped).
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, c, _] = v4.octets();
            !(v4.is_unspecified()
                || v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_multicast()
                || a == 0
                || (a == 100 && (64..128).contains(&b))
                || (a == 192 && b == 0 && c == 0)
                || (a == 198 && (b == 18 || b == 19))
                || a >= 240)
        }
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_public_ip(IpAddr::V4(v4)),
            None => {
                let s = v6.segments();
                !(v6.is_unspecified()
                    || v6.is_loopback()
                    || v6.is_multicast()
                    || (s[0] & 0xfe00) == 0xfc00
                    || (s[0] & 0xffc0) == 0xfe80
                    || (s[0] == 0x2001 && s[1] == 0x0db8)
                    || (s[0] == 0x0064 && s[1] == 0xff9b))
            }
        },
    }
}

fn public_only(
    host: &str,
    addrs: impl Iterator<Item = SocketAddr>,
) -> Result<Vec<SocketAddr>, String> {
    let addrs: Vec<SocketAddr> = addrs.filter(|a| is_public_ip(a.ip())).collect();
    if addrs.is_empty() {
        Err(format!("{host} has no public address"))
    } else {
        Ok(addrs)
    }
}

/// System DNS, keeping only public addresses (applied on every connection, redirects too).
struct PublicOnlyResolver;

impl reqwest::dns::Resolve for PublicOnlyResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        Box::pin(async move {
            let host = name.as_str().to_owned();
            let addrs = public_only(&host, tokio::net::lookup_host((host.as_str(), 0)).await?)?;
            let addrs: reqwest::dns::Addrs = Box::new(addrs.into_iter());
            Ok(addrs)
        })
    }
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
    fn url_parser_tricks_never_change_the_checked_host() {
        let p = producer(&["cdn.discordapp.com"], 10);
        for bad in [
            "https://cdn.discordapp.com@evil.test/a.mp3", // userinfo: host is evil.test
            "https://evil.test#@cdn.discordapp.com/a.mp3", // fragment
            "https://evil.test?@cdn.discordapp.com/a.mp3", // query
            "https://evil.test\\@cdn.discordapp.com/a.mp3", // backslash
            "https://cdn.discordapp.com.evil.test/a.mp3", // suffix
            "https://CDN.DISCORDAPP.COM.evil.test/",      // case + suffix
            "https://169.254.169.254/computeMetadata/v1/", // metadata, not listed
            "https://[::ffff:169.254.169.254]/",
        ] {
            assert!(check_url(&p, bad, false).is_err(), "{bad}");
        }
        // Case-insensitive host matching is the URL parser's normalization.
        assert!(check_url(&p, "https://CDN.discordapp.com/a.mp3", false).is_ok());
    }

    #[test]
    fn an_allowlisted_private_literal_is_still_refused() {
        let p = producer(&["169.254.169.254", "10.0.0.1", "8.8.8.8"], 10);
        assert!(check_url(&p, "https://169.254.169.254/x", false).is_err());
        assert!(check_url(&p, "https://10.0.0.1/x", false).is_err());
        assert!(check_url(&p, "https://8.8.8.8/x", false).is_ok());
    }

    #[test]
    fn dns_answers_are_filtered_to_public_addresses() {
        let a = |s: &str| s.parse::<SocketAddr>().unwrap();
        assert!(public_only("x", [a("127.0.0.1:0"), a("169.254.169.254:0")].into_iter()).is_err());
        assert_eq!(
            public_only("x", [a("10.1.1.1:0"), a("1.1.1.1:0")].into_iter()).unwrap(),
            [a("1.1.1.1:0")]
        );
    }

    #[tokio::test]
    async fn the_production_client_cannot_connect_to_a_name_with_only_private_answers() {
        // `localhost` resolves to loopback only: the resolver refuses it before connecting.
        let p = producer(&["localhost"], 10);
        let f = HttpsFetcher::new(Duration::from_secs(5)).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let err = f
            .fetch(&p, "https://localhost:9/a.mp3", &dir.path().join("in"))
            .await
            .unwrap_err();
        assert!(
            matches!(err, ProcessError::Transient(ref m) if m.contains("fetch")),
            "{err:?}"
        );
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
                )
                .route(
                    "/redirect-metadata",
                    get(|| async {
                        axum::response::Redirect::temporary(
                            "http://169.254.169.254/computeMetadata/v1/",
                        )
                    }),
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
        for hop in ["redirect-out", "redirect-metadata"] {
            let err = f
                .fetch(&p, &format!("{base}/{hop}"), &dest)
                .await
                .unwrap_err();
            assert!(
                matches!(err, ProcessError::Permanent { ref code, .. } if code == "policy_denied"),
                "{hop}: {err:?}"
            );
        }
    }
}
