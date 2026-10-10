//! `ObjectStore` on S3 (RustFS locally). Credentials come from the default
//! AWS chain: the ECS task role in production, `AWS_ACCESS_KEY_ID` /
//! `AWS_SECRET_ACCESS_KEY` locally. Nothing is fetched at construction.

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use aws_sdk_s3::Client;
use aws_sdk_s3::error::DisplayErrorContext;
use aws_sdk_s3::presigning::PresigningConfig;
use aws_sdk_s3::primitives::ByteStream;
use chrono::{DateTime, Utc};

use crate::application::ports::{ObjectStore, StoreError};

#[derive(Debug, Clone)]
pub struct S3Settings {
    pub bucket: String,
    /// Custom endpoint (RustFS/MinIO); path-style addressing when set.
    pub endpoint: Option<String>,
}

#[derive(Clone)]
pub struct S3Store {
    client: Client,
    bucket: String,
}

impl S3Store {
    pub async fn new(settings: S3Settings) -> Self {
        let shared = aws_config::load_from_env().await;
        let mut config = aws_sdk_s3::config::Builder::from(&shared);
        if let Some(endpoint) = &settings.endpoint {
            config = config.endpoint_url(endpoint).force_path_style(true);
        }
        Self {
            client: Client::from_conf(config.build()),
            bucket: settings.bucket,
        }
    }

    /// Local development only: create the bucket if it doesn't exist.
    pub async fn ensure_bucket(&self) -> Result<(), StoreError> {
        if self
            .client
            .head_bucket()
            .bucket(&self.bucket)
            .send()
            .await
            .is_ok()
        {
            return Ok(());
        }
        self.client
            .create_bucket()
            .bucket(&self.bucket)
            .send()
            .await
            .map(|_| tracing::info!(bucket = %self.bucket, "created library bucket"))
            .map_err(error)
    }
}

fn error<E: std::error::Error>(err: E) -> StoreError {
    StoreError(DisplayErrorContext(&err).to_string())
}

fn to_chrono(time: &aws_sdk_s3::primitives::DateTime) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp(time.secs(), time.subsec_nanos())
}

#[async_trait]
impl ObjectStore for S3Store {
    async fn put_file(&self, key: &str, path: &Path) -> Result<(), StoreError> {
        let body = ByteStream::from_path(path).await.map_err(error)?;
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .content_type("audio/ogg")
            .body(body)
            .send()
            .await
            .map_err(error)?;
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), StoreError> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(error)?;
        Ok(())
    }

    async fn last_modified(&self, key: &str) -> Result<Option<DateTime<Utc>>, StoreError> {
        match self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
        {
            Ok(head) => Ok(head.last_modified().and_then(to_chrono)),
            Err(err) if err.as_service_error().is_some_and(|e| e.is_not_found()) => Ok(None),
            Err(err) => Err(error(err)),
        }
    }

    async fn list(&self, prefix: &str) -> Result<Vec<(String, DateTime<Utc>)>, StoreError> {
        let mut pages = self
            .client
            .list_objects_v2()
            .bucket(&self.bucket)
            .prefix(prefix)
            .into_paginator()
            .send();
        let mut objects = Vec::new();
        while let Some(page) = pages.next().await {
            for object in page.map_err(error)?.contents() {
                if let (Some(key), Some(modified)) =
                    (object.key(), object.last_modified().and_then(to_chrono))
                {
                    objects.push((key.to_owned(), modified));
                }
            }
        }
        Ok(objects)
    }

    async fn presign_get(&self, key: &str, ttl: Duration) -> Result<String, StoreError> {
        let config = PresigningConfig::expires_in(ttl).map_err(error)?;
        let request = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .presigned(config)
            .await
            .map_err(error)?;
        Ok(request.uri().to_owned())
    }
}

/// Against the Compose RustFS: `make -C discord_bot test-db`.
#[cfg(test)]
mod tests {
    use super::*;

    fn rustfs() -> Option<S3Settings> {
        let endpoint = std::env::var("MUSIC_S3_ENDPOINT").ok()?;
        Some(S3Settings {
            bucket: std::env::var("MUSIC_S3_BUCKET").unwrap_or_else(|_| "tatarbot-test".into()),
            endpoint: Some(endpoint),
        })
    }

    #[tokio::test]
    #[ignore = "needs RustFS (make test-db)"]
    async fn s3_round_trip_and_presign() {
        let store = S3Store::new(rustfs().expect("MUSIC_S3_ENDPOINT")).await;
        store.ensure_bucket().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.ogg");
        tokio::fs::write(&file, b"OggS-test").await.unwrap();
        let key = "test/round-trip.ogg";

        store.put_file(key, &file).await.unwrap();
        assert!(store.last_modified(key).await.unwrap().is_some());
        assert!(
            store
                .list("test/")
                .await
                .unwrap()
                .iter()
                .any(|(k, _)| k == key)
        );

        let url = store
            .presign_get(key, Duration::from_secs(60))
            .await
            .unwrap();
        let body = reqwest_audio::get(&url)
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert_eq!(&body[..], b"OggS-test");

        store.delete(key).await.unwrap();
        assert_eq!(store.last_modified(key).await.unwrap(), None);
    }
}
