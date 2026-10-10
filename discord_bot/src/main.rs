//! Composition root: the only place that knows about every concrete type.

mod adapters;
mod application;
mod config;
mod domain;
mod error;

use std::sync::Arc;

use adapters::inbound::discord;
use adapters::outbound::agent_http::HttpAgentClient;
use adapters::outbound::audio_http;
use adapters::outbound::songbird_voice::SongbirdVoice;
use adapters::outbound::url_resolver::UrlTrackResolver;
use application::ports::{ChatService, MusicService};
use application::use_cases::{ChatUseCase, MusicUseCase};
use config::Config;
use error::AppError;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), AppError> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,serenity=warn")),
        )
        .init();

    let config = Config::from_env()?;

    // Outbound adapter -> use case -> inbound port.
    let agent = HttpAgentClient::new(
        &config.agent.base_url,
        config.agent.api_key.clone(),
        config.agent.timeout,
    )?;
    let chat: Arc<dyn ChatService> = Arc::new(ChatUseCase::new(agent));

    // Music: Songbird voice + direct links; independent of the Agent Runtime.
    let songbird = songbird::Songbird::serenity();
    let voice = SongbirdVoice::new(songbird.clone(), audio_http::client()?);
    let stations = config.music.stations.iter().count();
    let music: Arc<dyn MusicService> = Arc::new(MusicUseCase::new(
        voice,
        UrlTrackResolver,
        config.music.stations,
        config.music.max_queue,
    ));

    tracing::info!(
        agent = %config.agent.base_url,
        auth = config.agent.api_key.is_some(),
        stations,
        "starting TatarBot"
    );
    discord::run(config.discord, chat, music, songbird).await
}
