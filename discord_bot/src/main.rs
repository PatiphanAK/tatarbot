//! Composition root: the only place that knows about every concrete type.

mod adapters;
mod application;
mod config;
mod domain;
mod error;
mod library;
mod tls;

use std::sync::Arc;

use adapters::inbound::discord;
use adapters::outbound::agent_http::HttpAgentClient;
use adapters::outbound::audio_http;
use adapters::outbound::library_resolver::LibraryTrackResolver;
use adapters::outbound::songbird_voice::{LibraryAudio, SongbirdVoice};
use adapters::outbound::url_resolver::UrlTrackResolver;
use application::ports::{ChatService, MusicService, TrackResolver};
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

    // Before anything opens a TLS connection (see tls.rs).
    tls::install_crypto_provider()?;

    let config = Config::from_env()?;

    // Outbound adapter -> use case -> inbound port.
    let agent = HttpAgentClient::new(
        &config.agent.base_url,
        config.agent.api_key.clone(),
        config.agent.timeout,
    )?;
    let chat: Arc<dyn ChatService> = Arc::new(ChatUseCase::new(agent));

    // Server library (optional): Postgres metadata, S3 audio, ffmpeg.
    let library = match config.library {
        Some(cfg) => Some(library::build(cfg).await?),
        None => None,
    };

    // Music: Songbird voice, stations and links; independent of the Agent Runtime.
    let songbird = songbird::Songbird::serenity();
    let library_audio = match &library {
        Some(lib) => Some(Arc::new(LibraryAudio {
            store: lib.store.clone(),
            http: audio_http::trusted_client()?,
        })),
        None => None,
    };
    let voice = SongbirdVoice::new(songbird.clone(), audio_http::client()?, library_audio);
    let resolver: Arc<dyn TrackResolver> = match &library {
        Some(lib) => Arc::new(LibraryTrackResolver::new(lib.service.clone())),
        None => Arc::new(UrlTrackResolver),
    };
    let stations = config.music.stations.iter().count();
    let music: Arc<dyn MusicService> = Arc::new(MusicUseCase::new(
        voice,
        resolver,
        config.music.stations,
        config.music.max_queue,
    ));

    tracing::info!(
        agent = %config.agent.base_url,
        auth = config.agent.api_key.is_some(),
        stations,
        library = library.is_some(),
        "starting TatarBot"
    );
    let library_commands = library.map(|lib| discord::LibraryCommands {
        service: lib.service,
        upload_role: lib.upload_role,
    });
    discord::run(config.discord, chat, music, library_commands, songbird).await
}
