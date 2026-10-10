use std::env;
use std::fmt;
use std::time::Duration;

use reqwest::Url;

use crate::domain::music::StationList;
use crate::error::AppError;

const DEFAULT_STATIONS: &str = include_str!("../stations.toml");

#[derive(Debug, Clone)]
pub struct Config {
    pub discord: DiscordConfig,
    pub agent: AgentConfig,
    pub music: MusicConfig,
    /// The server library; `None` unless both DATABASE_URL and
    /// MUSIC_S3_BUCKET are set.
    pub library: Option<LibraryConfig>,
}

#[derive(Clone)]
pub struct LibraryConfig {
    pub database_url: String,
    pub max_connections: u32,
    pub bucket: String,
    pub prefix: String,
    pub endpoint: Option<String>,
    /// Local development: create the bucket if missing.
    pub create_bucket: bool,
    /// Role name (per server) that may upload and delete others' tracks.
    pub upload_role: String,
    pub quota_bytes: u64,
    pub ffmpeg: std::path::PathBuf,
    pub ffprobe: std::path::PathBuf,
}

impl LibraryConfig {
    fn from_env() -> Result<Option<Self>, AppError> {
        let (database_url, bucket) = match (optional("DATABASE_URL"), optional("MUSIC_S3_BUCKET")) {
            (None, None) => return Ok(None),
            (Some(url), Some(bucket)) => (url, bucket),
            (Some(_), None) => return Err(AppError::MissingEnv("MUSIC_S3_BUCKET")),
            (None, Some(_)) => return Err(AppError::MissingEnv("DATABASE_URL")),
        };
        let prefix = optional("MUSIC_S3_PREFIX").unwrap_or_else(|| "library/".to_owned());
        if !prefix.ends_with('/') || prefix.starts_with('/') {
            return Err(invalid(
                "MUSIC_S3_PREFIX",
                "must end with / and not start with /",
            ));
        }
        let quota_mb = parse_range("MUSIC_GUILD_QUOTA_MB", 1..=100_000, 500)?;
        Ok(Some(Self {
            database_url,
            max_connections: parse_range("DATABASE_MAX_CONNECTIONS", 1..=20, 3)? as u32,
            bucket,
            prefix,
            endpoint: optional("MUSIC_S3_ENDPOINT"),
            create_bucket: optional("MUSIC_S3_CREATE_BUCKET").is_some_and(|v| v == "true"),
            upload_role: optional("MUSIC_UPLOAD_ROLE").unwrap_or_else(|| "DJ".to_owned()),
            quota_bytes: quota_mb * 1024 * 1024,
            ffmpeg: optional("FFMPEG_PATH")
                .unwrap_or_else(|| "ffmpeg".into())
                .into(),
            ffprobe: optional("FFPROBE_PATH")
                .unwrap_or_else(|| "ffprobe".into())
                .into(),
        }))
    }
}

impl fmt::Debug for LibraryConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LibraryConfig")
            .field("database_url", &"<redacted>")
            .field("bucket", &self.bucket)
            .field("prefix", &self.prefix)
            .field("endpoint", &self.endpoint)
            .field("upload_role", &self.upload_role)
            .field("quota_bytes", &self.quota_bytes)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
pub struct MusicConfig {
    pub stations: StationList,
    /// Tracks per guild, including the one playing.
    pub max_queue: usize,
}

#[derive(Clone)]
pub struct DiscordConfig {
    pub token: String,
}

/// Connection to the Agent Runtime. TatarBot knows nothing about models or
/// inference backends; that is the runtime's concern.
#[derive(Clone)]
pub struct AgentConfig {
    pub base_url: Url,
    /// Sent as `Authorization: Bearer <key>` when set.
    pub api_key: Option<String>,
    pub timeout: Duration,
}

impl Config {
    pub fn from_env() -> Result<Self, AppError> {
        let token = required("DISCORD_TOKEN")?;

        let base_url =
            optional("AGENT_BASE_URL").unwrap_or_else(|| "http://localhost:8080".to_owned());
        let base_url =
            Url::parse(&base_url).map_err(|e| invalid("AGENT_BASE_URL", &e.to_string()))?;
        let timeout_secs = optional("AGENT_TIMEOUT_SECS")
            .map(|v| {
                v.parse::<u64>()
                    .map_err(|_| invalid("AGENT_TIMEOUT_SECS", "must be an integer"))
            })
            .transpose()?
            .unwrap_or(120);

        let stations_text = match optional("MUSIC_STATIONS_FILE") {
            Some(path) => std::fs::read_to_string(&path)
                .map_err(|e| invalid("MUSIC_STATIONS_FILE", &format!("{path}: {e}")))?,
            None => DEFAULT_STATIONS.to_owned(),
        };
        let stations = StationList::from_toml(&stations_text)
            .map_err(|e| invalid("MUSIC_STATIONS_FILE", &e.to_string()))?;
        let max_queue = optional("MUSIC_MAX_QUEUE")
            .map(|v| match v.parse::<usize>() {
                Ok(n @ 1..=500) => Ok(n),
                _ => Err(invalid(
                    "MUSIC_MAX_QUEUE",
                    "must be an integer from 1 to 500",
                )),
            })
            .transpose()?
            .unwrap_or(50);

        Ok(Self {
            library: LibraryConfig::from_env()?,
            music: MusicConfig {
                stations,
                max_queue,
            },
            discord: DiscordConfig { token },
            agent: AgentConfig {
                base_url,
                api_key: optional("AGENT_API_KEY"),
                timeout: Duration::from_secs(timeout_secs),
            },
        })
    }
}

// Manual Debug impls keep secrets out of logs.
impl fmt::Debug for DiscordConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DiscordConfig")
            .field("token", &"<redacted>")
            .finish()
    }
}

impl fmt::Debug for AgentConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentConfig")
            .field("base_url", &self.base_url.as_str())
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("timeout", &self.timeout)
            .finish()
    }
}

fn optional(key: &str) -> Option<String> {
    env::var(key).ok().filter(|v| !v.trim().is_empty())
}

fn required(key: &'static str) -> Result<String, AppError> {
    optional(key).ok_or(AppError::MissingEnv(key))
}

fn parse_range(
    key: &'static str,
    range: std::ops::RangeInclusive<u64>,
    default: u64,
) -> Result<u64, AppError> {
    match optional(key) {
        None => Ok(default),
        Some(v) => v
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|n| range.contains(n))
            .ok_or_else(|| {
                invalid(
                    key,
                    &format!(
                        "must be an integer from {} to {}",
                        range.start(),
                        range.end()
                    ),
                )
            }),
    }
}

fn invalid(key: &'static str, reason: &str) -> AppError {
    AppError::InvalidEnv {
        key,
        reason: reason.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_output_redacts_secrets() {
        let agent = AgentConfig {
            base_url: Url::parse("http://agent").unwrap(),
            api_key: Some("super-secret".into()),
            timeout: Duration::from_secs(1),
        };
        let discord = DiscordConfig {
            token: "discord-secret".into(),
        };
        let library = LibraryConfig {
            database_url: "postgres://u:db-secret@h/d".into(),
            max_connections: 3,
            bucket: "b".into(),
            prefix: "library/".into(),
            endpoint: None,
            create_bucket: false,
            upload_role: "DJ".into(),
            quota_bytes: 1,
            ffmpeg: "ffmpeg".into(),
            ffprobe: "ffprobe".into(),
        };
        let out = format!("{agent:?} {discord:?} {library:?}");
        assert!(!out.contains("super-secret") && !out.contains("discord-secret"));
        assert!(!out.contains("db-secret"));
    }
}
