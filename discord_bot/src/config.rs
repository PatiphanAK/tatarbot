use std::env;
use std::fmt;
use std::time::Duration;

use reqwest::Url;

use crate::error::AppError;

#[derive(Debug, Clone)]
pub struct Config {
    pub discord: DiscordConfig,
    pub agent: AgentConfig,
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

        Ok(Self {
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
        let out = format!("{agent:?} {discord:?}");
        assert!(!out.contains("super-secret") && !out.contains("discord-secret"));
    }
}
