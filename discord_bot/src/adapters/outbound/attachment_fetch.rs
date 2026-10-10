//! `AttachmentFetcher`: downloads a Discord attachment to a local file while
//! hashing it. Only Discord's CDN hosts are accepted, through the
//! public-address-only client, and the body is cut off at `max_bytes`
//! whatever `Content-Length` claims.

use std::path::Path;

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use url::Url;

use crate::application::ports::{AttachmentFetcher, FetchError, Fetched};
use crate::domain::library::ContentHash;

const DISCORD_HOSTS: [&str; 2] = ["cdn.discordapp.com", "media.discordapp.net"];

pub struct HttpAttachmentFetcher {
    client: reqwest_audio::Client,
}

impl HttpAttachmentFetcher {
    /// `client` should be `audio_http::client()` (public addresses only).
    pub fn new(client: reqwest_audio::Client) -> Self {
        Self { client }
    }
}

pub fn is_discord_attachment(url: &str) -> bool {
    Url::parse(url).is_ok_and(|u| {
        u.scheme() == "https" && u.host_str().is_some_and(|h| DISCORD_HOSTS.contains(&h))
    })
}

#[async_trait]
impl AttachmentFetcher for HttpAttachmentFetcher {
    async fn fetch(&self, url: &str, max_bytes: u64, dest: &Path) -> Result<Fetched, FetchError> {
        if !is_discord_attachment(url) {
            return Err(FetchError::NotAllowed);
        }
        let mut response = self
            .client
            .get(url)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| FetchError::Failed(e.without_url().to_string()))?;
        if response.content_length().is_some_and(|len| len > max_bytes) {
            return Err(FetchError::TooLarge);
        }
        copy_capped(&mut response, max_bytes, dest).await
    }
}

/// Anything that yields body chunks (a reqwest response, or a test double).
#[async_trait]
trait Chunks: Send {
    async fn next_chunk(&mut self) -> Result<Option<bytes::Bytes>, String>;
}

#[async_trait]
impl Chunks for reqwest_audio::Response {
    async fn next_chunk(&mut self) -> Result<Option<bytes::Bytes>, String> {
        self.chunk().await.map_err(|e| e.without_url().to_string())
    }
}

async fn copy_capped(
    body: &mut dyn Chunks,
    max_bytes: u64,
    dest: &Path,
) -> Result<Fetched, FetchError> {
    let failed = |e: std::io::Error| FetchError::Failed(e.to_string());
    let mut file = tokio::fs::File::create(dest).await.map_err(failed)?;
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    while let Some(chunk) = body.next_chunk().await.map_err(FetchError::Failed)? {
        size += chunk.len() as u64;
        if size > max_bytes {
            return Err(FetchError::TooLarge);
        }
        hasher.update(&chunk);
        file.write_all(&chunk).await.map_err(failed)?;
    }
    file.flush().await.map_err(failed)?;
    Ok(Fetched {
        size_bytes: size,
        hash: ContentHash::from_digest(&hasher.finalize()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed(Vec<&'static [u8]>);

    #[async_trait]
    impl Chunks for Fixed {
        async fn next_chunk(&mut self) -> Result<Option<bytes::Bytes>, String> {
            Ok((!self.0.is_empty()).then(|| bytes::Bytes::from_static(self.0.remove(0))))
        }
    }

    #[test]
    fn only_discord_cdn_urls_are_accepted() {
        assert!(is_discord_attachment(
            "https://cdn.discordapp.com/attachments/1/2/song.mp3?ex=1"
        ));
        assert!(is_discord_attachment(
            "https://media.discordapp.net/attachments/1/2/a.ogg"
        ));
        for url in [
            "http://cdn.discordapp.com/attachments/1/2/a.mp3",
            "https://cdn.discordapp.com.evil.example/a.mp3",
            "https://evil.example/cdn.discordapp.com/a.mp3",
            "file:///etc/passwd",
        ] {
            assert!(!is_discord_attachment(url), "{url}");
        }
    }

    #[tokio::test]
    async fn copies_and_hashes() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("f");
        let fetched = copy_capped(&mut Fixed(vec![b"ab", b"c"]), 10, &dest)
            .await
            .unwrap();
        assert_eq!(fetched.size_bytes, 3);
        // sha256("abc")
        assert_eq!(
            fetched.hash.as_str(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(tokio::fs::read(&dest).await.unwrap(), b"abc");
    }

    #[tokio::test]
    async fn stops_at_the_cap_even_without_content_length() {
        let dir = tempfile::tempdir().unwrap();
        let result =
            copy_capped(&mut Fixed(vec![b"12345", b"678"]), 6, &dir.path().join("f")).await;
        assert_eq!(result, Err(FetchError::TooLarge));
    }

    #[tokio::test]
    async fn refuses_other_hosts_without_a_request() {
        let fetcher = HttpAttachmentFetcher::new(reqwest_audio::Client::new());
        let dir = tempfile::tempdir().unwrap();
        let result = fetcher
            .fetch("https://evil.example/a.mp3", 10, &dir.path().join("f"))
            .await;
        assert_eq!(result, Err(FetchError::NotAllowed));
    }
}
