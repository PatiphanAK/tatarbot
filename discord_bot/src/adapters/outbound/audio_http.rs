//! HTTP client for audio streams (Songbird inputs), safe for user-supplied URLs.
//!
//! Users can make the bot fetch any URL, so the client must not reach private
//! networks: the ECS task metadata/credentials endpoint (169.254.170.2), VPC
//! addresses, localhost. Every hostname is resolved through
//! `PublicOnlyResolver`, which drops non-public addresses, so DNS rebinding
//! gains nothing; IP-literal hosts never hit a resolver, so they are checked
//! on the request URL (`UrlTrackResolver`) and on every redirect here.
//!
//! Songbird takes a reqwest 0.12 client (`reqwest-audio` in Cargo.toml).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use reqwest_audio::dns::{Addrs, Name, Resolve, Resolving};
use reqwest_audio::redirect;
use url::{Host, Url};

/// Some stream hosts (e.g. SomaFM) refuse generic clients but serve a
/// descriptive agent.
pub const USER_AGENT: &str = concat!(
    "TatarBot/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/PatiphanAK/tatarbot)"
);
const MAX_REDIRECTS: usize = 5;

pub fn client() -> Result<reqwest_audio::Client, reqwest_audio::Error> {
    reqwest_audio::Client::builder()
        .user_agent(USER_AGENT)
        .dns_resolver(Arc::new(PublicOnlyResolver))
        .redirect(redirect::Policy::custom(|attempt| match redirect_decision(
            attempt.url(),
            attempt.previous().len(),
        ) {
            Ok(()) => attempt.follow(),
            Err(reason) => attempt.error(reason),
        }))
        .connect_timeout(Duration::from_secs(10))
        // No total timeout: live streams never end. A stalled one is cut off.
        .read_timeout(Duration::from_secs(30))
        .https_only(false)
        .build()
}

/// For presigned library URLs (our own bucket), without the public-address
/// guard: the bucket is on localhost/a private network in development.
pub fn trusted_client() -> Result<reqwest_audio::Client, reqwest_audio::Error> {
    reqwest_audio::Client::builder()
        .user_agent(USER_AGENT)
        .redirect(redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(30))
        .build()
}

/// Whether a redirect may be followed: a bounded number of hops, each to a public host.
/// (Hostnames are additionally filtered by `PublicOnlyResolver` when connecting.)
fn redirect_decision(next: &Url, hops_so_far: usize) -> Result<(), &'static str> {
    if hops_so_far >= MAX_REDIRECTS {
        Err("too many redirects")
    } else if !matches!(next.scheme(), "http" | "https") {
        Err("redirect to a non-HTTP scheme")
    } else if !is_public_url(next) {
        Err("redirect to a non-public address")
    } else {
        Ok(())
    }
}

/// Keeps only public addresses; none left is an error. Applied to every DNS answer at
/// connect time, so a name that resolves (or re-resolves) to a private address can't be
/// used to reach it: DNS rebinding gains nothing.
fn public_only(
    host: &str,
    addrs: impl Iterator<Item = SocketAddr>,
) -> Result<Vec<SocketAddr>, String> {
    let addrs: Vec<SocketAddr> = addrs.filter(|addr| is_public_ip(addr.ip())).collect();
    if addrs.is_empty() {
        Err(format!("{host} has no public address"))
    } else {
        Ok(addrs)
    }
}

/// False for URLs whose host is an IP literal (or `localhost`) outside the
/// public internet. Hostnames are left to `PublicOnlyResolver`.
pub fn is_public_url(url: &Url) -> bool {
    match url.host() {
        Some(Host::Ipv4(ip)) => is_public_ip(IpAddr::V4(ip)),
        Some(Host::Ipv6(ip)) => is_public_ip(IpAddr::V6(ip)),
        Some(Host::Domain(domain)) => {
            let domain = domain.trim_end_matches('.').to_ascii_lowercase();
            domain != "localhost" && !domain.ends_with(".localhost")
        }
        None => false,
    }
}

pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_v4(ip),
        IpAddr::V6(ip) => match ip.to_ipv4_mapped() {
            Some(v4) => is_public_v4(v4),
            None => is_public_v6(ip),
        },
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local() // incl. 169.254.169.254 and ECS 169.254.170.2
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
        || a == 0
        || (a == 100 && (64..128).contains(&b)) // shared address space (CGNAT)
        || (a == 192 && b == 0 && c == 0) // IETF protocol assignments
        || (a == 198 && (b == 18 || b == 19)) // benchmarking
        || a >= 240) // reserved
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    let first = ip.segments()[0];
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        || (first & 0xfe00) == 0xfc00 // unique local
        || (first & 0xffc0) == 0xfe80 // link local
        || (first == 0x2001 && ip.segments()[1] == 0x0db8) // documentation
        || (first == 0x0064 && ip.segments()[1] == 0xff9b)) // NAT64 to IPv4
}

/// Resolves with the system resolver and keeps only public addresses.
struct PublicOnlyResolver;

impl Resolve for PublicOnlyResolver {
    fn resolve(&self, name: Name) -> Resolving {
        Box::pin(async move {
            let host = name.as_str().to_owned();
            let addrs = public_only(&host, tokio::net::lookup_host((host.as_str(), 0)).await?)?;
            let addrs: Addrs = Box::new(addrs.into_iter());
            Ok(addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_addresses() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "169.254.170.2",
            "100.64.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "240.0.0.1",
            "::1",
            "::",
            "fd00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "64:ff9b::a00:1",
        ] {
            assert!(!is_public_ip(ip.parse().unwrap()), "{ip} should be private");
        }
        for ip in [
            "1.1.1.1",
            "8.8.8.8",
            "2606:4700:4700::1111",
            "::ffff:1.1.1.1",
        ] {
            assert!(is_public_ip(ip.parse().unwrap()), "{ip} should be public");
        }
    }

    #[test]
    fn classifies_urls() {
        for url in [
            "http://127.0.0.1/a",
            "http://[::1]:8080/",
            "http://169.254.170.2/v2/credentials",
            "http://localhost/x",
            "http://api.localhost./x",
            "http://2130706433/", // 127.0.0.1 as an integer; url normalises it
        ] {
            assert!(!is_public_url(&Url::parse(url).unwrap()), "{url}");
        }
        assert!(is_public_url(
            &Url::parse("https://ice2.somafm.com/fluid-128-mp3").unwrap()
        ));
    }

    #[tokio::test]
    async fn resolver_refuses_names_that_only_resolve_privately() {
        let err = PublicOnlyResolver
            .resolve("localhost".parse().unwrap())
            .await
            .err()
            .expect("localhost must not resolve");
        assert!(err.to_string().contains("no public address"));
    }

    /// Obfuscated IP literals: the URL parser normalizes them, so the check sees the real
    /// address (hex, octal, short forms, IPv4-mapped IPv6, metadata endpoints).
    #[test]
    fn obfuscated_private_literals_are_refused() {
        for url in [
            "http://0x7f.0.0.1/",
            "http://0177.0.0.1/",
            "http://127.1/",
            "http://0x7f000001/",
            "http://[::ffff:127.0.0.1]/",
            "http://[0:0:0:0:0:ffff:7f00:1]/",
            "http://[::ffff:a9fe:a9fe]/latest/meta-data", // 169.254.169.254
            "http://169.254.169.254/computeMetadata/v1/",
            "http://[fd00:ec2::254]/",
            "http://10.0.0.1:8080/",
            "http://LOCALHOST./",
        ] {
            assert!(!is_public_url(&Url::parse(url).unwrap()), "{url}");
        }
    }

    /// DNS rebinding: whatever a name resolves to at connect time, private answers are
    /// dropped; a name with only private answers can't be connected to at all.
    #[test]
    fn rebinding_answers_are_filtered_at_connect_time() {
        let addr = |s: &str| s.parse::<SocketAddr>().unwrap();
        let rebound = [addr("169.254.169.254:0"), addr("127.0.0.1:0")];
        assert!(public_only("evil.test", rebound.into_iter()).is_err());
        let mixed = [addr("10.0.0.5:0"), addr("93.184.216.34:0"), addr("[::1]:0")];
        assert_eq!(
            public_only("evil.test", mixed.into_iter()).unwrap(),
            [addr("93.184.216.34:0")]
        );
    }

    #[test]
    fn redirects_are_bounded_and_public_only() {
        let url = |s: &str| Url::parse(s).unwrap();
        assert!(redirect_decision(&url("https://cdn.example.com/a.mp3"), 0).is_ok());
        for bad in [
            "http://127.0.0.1/",
            "http://169.254.169.254/latest/meta-data/",
            "http://[::1]/",
            "http://localhost:8080/",
            "file:///etc/passwd",
            "gopher://10.0.0.1:6379/_FLUSHALL",
        ] {
            assert!(redirect_decision(&url(bad), 0).is_err(), "{bad}");
        }
        assert_eq!(
            redirect_decision(&url("https://cdn.example.com/a.mp3"), MAX_REDIRECTS),
            Err("too many redirects")
        );
    }

    /// End to end through the guarded client: a redirect hop to a private address is not
    /// followed. (The test server is on loopback; IP-literal hosts don't go through the
    /// resolver, which is why `UrlTrackResolver` refuses them on the request URL. Here the
    /// first hop is allowed only to exercise the redirect policy.)
    #[tokio::test]
    async fn the_guarded_client_refuses_loopback_and_private_redirect_targets() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let router = axum::Router::new().route(
                "/hop",
                axum::routing::get(|| async {
                    axum::response::Redirect::temporary("http://169.254.169.254/latest/meta-data")
                }),
            );
            axum::serve(listener, router).await.unwrap();
        });
        // A loopback request URL is refused before any request (UrlTrackResolver's check).
        assert!(!is_public_url(&Url::parse(&base).unwrap()));
        // A redirect hop to the metadata server: the guarded client errors instead of following.
        let err = client()
            .unwrap()
            .get(format!("{base}/hop"))
            .send()
            .await
            .unwrap_err();
        assert!(err.is_redirect(), "{err}");
    }

    #[test]
    fn client_builds() {
        client().unwrap();
    }
}
