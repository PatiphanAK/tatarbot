//! `ObjectStore` adapters for GCS without HMAC (ADR 0004 §5).
//!
//! * `GcsObjectStore` (worker): its own service account through the metadata server. It
//!   never uploads through `put_file`: the ingest core already wrote the object
//!   (create-only), so `put_file` only confirms it exists, under the library's hash lock.
//! * `GcsReadStore` (bot, phase 3): playback with a downscoped read-only bearer token from
//!   a file the host refreshes. `presign_get` returns the plain JSON-API media URL; the token
//!   goes in a header (songbird `HttpRequest::new_with_headers`). Deletes are deferred to the
//!   worker's sweep, because the bot can't write.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ingest_core::gcp::GcsClient;

use crate::application::ports::{ObjectStore, StoreError};

pub struct GcsObjectStore {
    client: Arc<GcsClient>,
}

impl GcsObjectStore {
    pub fn new(client: Arc<GcsClient>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl ObjectStore for GcsObjectStore {
    /// Confirms the object the core uploaded is (still) there; fails otherwise, which makes the
    /// job retry and upload it again.
    async fn put_file(&self, key: &str, _path: &Path) -> Result<(), StoreError> {
        match self.client.updated(key).await.map_err(StoreError)? {
            Some(_) => Ok(()),
            None => Err(StoreError(format!(
                "{key} is missing; it will be uploaded again"
            ))),
        }
    }

    async fn delete(&self, key: &str) -> Result<(), StoreError> {
        self.client.delete(key).await.map_err(StoreError)
    }

    async fn last_modified(&self, key: &str) -> Result<Option<DateTime<Utc>>, StoreError> {
        self.client.updated(key).await.map_err(StoreError)
    }

    async fn list(&self, prefix: &str) -> Result<Vec<(String, DateTime<Utc>)>, StoreError> {
        self.client.list(prefix).await.map_err(StoreError)
    }

    async fn presign_get(&self, _key: &str, _ttl: Duration) -> Result<String, StoreError> {
        Err(StoreError("the worker never serves playback".into()))
    }
}

pub struct GcsReadStore {
    bucket: String,
    api_base: String,
}

impl GcsReadStore {
    pub fn new(bucket: impl Into<String>) -> Self {
        Self {
            bucket: bucket.into(),
            api_base: "https://storage.googleapis.com".into(),
        }
    }

    #[doc(hidden)]
    pub fn with_api_base(mut self, base: impl Into<String>) -> Self {
        self.api_base = base.into();
        self
    }
}

#[async_trait]
impl ObjectStore for GcsReadStore {
    async fn put_file(&self, _key: &str, _path: &Path) -> Result<(), StoreError> {
        Err(StoreError(
            "read-only store: uploads go through the ingest worker".into(),
        ))
    }

    /// Deferred: the row is gone, and the worker's sweep removes the orphaned object after
    /// the grace period.
    async fn delete(&self, key: &str) -> Result<(), StoreError> {
        tracing::debug!(key, "object delete deferred to the ingest worker's sweep");
        Ok(())
    }

    async fn last_modified(&self, _key: &str) -> Result<Option<DateTime<Utc>>, StoreError> {
        Ok(None)
    }

    async fn list(&self, _prefix: &str) -> Result<Vec<(String, DateTime<Utc>)>, StoreError> {
        Ok(Vec::new())
    }

    async fn presign_get(&self, key: &str, _ttl: Duration) -> Result<String, StoreError> {
        Ok(format!(
            "{}/storage/v1/b/{}/o/{}?alt=media",
            self.api_base,
            self.bucket,
            url::form_urlencoded::byte_serialize(key.as_bytes()).collect::<String>()
        ))
    }
}

/// Reads a bearer token file the host keeps fresh; re-read on every use.
pub async fn read_token(path: &Path) -> Result<String, StoreError> {
    let token = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| StoreError(format!("token file {}: {e}", path.display())))?;
    let token = token.trim();
    if token.is_empty() || token.contains(char::is_whitespace) {
        return Err(StoreError(format!(
            "token file {} is empty or malformed",
            path.display()
        )));
    }
    Ok(token.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn read_store_builds_media_urls_and_defers_deletes() {
        let store = GcsReadStore::new("bkt");
        assert_eq!(
            store
                .presign_get("library/ab.ogg", Duration::ZERO)
                .await
                .unwrap(),
            "https://storage.googleapis.com/storage/v1/b/bkt/o/library%2Fab.ogg?alt=media"
        );
        assert!(store.delete("library/ab.ogg").await.is_ok());
        assert!(store.put_file("k", Path::new("/x")).await.is_err());
    }

    #[tokio::test]
    async fn token_files_must_hold_one_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t");
        tokio::fs::write(&path, "ya29.abc\n").await.unwrap();
        assert_eq!(read_token(&path).await.unwrap(), "ya29.abc");
        tokio::fs::write(&path, "").await.unwrap();
        assert!(read_token(&path).await.is_err());
        assert!(read_token(&dir.path().join("missing")).await.is_err());
    }
}
