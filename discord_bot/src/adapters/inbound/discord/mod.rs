//! Inbound adapter: turns Discord messages addressed to the bot into calls
//! on the `ChatService` port and renders results back as Discord messages.
//!
//! Everything Discord-specific (IDs, intents, message limits, mention
//! handling, error wording) stays inside this module.

mod convert;
mod handler;
mod music;
mod presenter;

use std::sync::Arc;
use std::time::Duration;

use serenity::all::{Client, GatewayError, GatewayIntents};
use songbird::{SerenityInit, Songbird};

use crate::adapters::outbound::songbird_voice;
use crate::application::ports::{ChatService, MusicService};
use crate::config::DiscordConfig;
use crate::error::AppError;

/// Well below ECS's stopTimeout and Docker's 10 s default before SIGKILL.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CONNECT_BACKOFF: Duration = Duration::from_secs(60);

pub async fn run(
    config: DiscordConfig,
    chat: Arc<dyn ChatService>,
    music: Arc<dyn MusicService>,
    voice: Arc<Songbird>,
) -> Result<(), AppError> {
    // MESSAGE_CONTENT (privileged) is required to read replies to the bot
    // that don't ping it; enable it under Bot -> Privileged Gateway Intents.
    // GUILDS + GUILD_VOICE_STATES (not privileged) let the cache know which
    // voice channel a user is in, and drive Songbird's connections.
    let intents = GatewayIntents::GUILDS
        | GatewayIntents::GUILD_MESSAGES
        | GatewayIntents::DIRECT_MESSAGES
        | GatewayIntents::MESSAGE_CONTENT
        | GatewayIntents::GUILD_VOICE_STATES;

    let mut client = Client::builder(&config.token, intents)
        .event_handler(handler::Handler::new(chat, music))
        .register_songbird_with(voice.clone())
        .await?;

    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    tokio::select! {
        result = wait_for_discord(&client.http) => result?,
        () = &mut shutdown => {
            tracing::info!("shutting down");
            return Ok(());
        }
    }

    let shard_manager = client.shard_manager.clone();
    let result = tokio::select! {
        result = client.start() => result,
        () = &mut shutdown => {
            tracing::info!("shutting down");
            // Leave voice channels while the gateway is still up.
            if tokio::time::timeout(SHUTDOWN_TIMEOUT, songbird_voice::leave_all(&voice))
                .await
                .is_err()
            {
                tracing::warn!("voice connections did not close in time");
            }
            // Bounded: shards that are reconnecting may never acknowledge the close.
            if tokio::time::timeout(SHUTDOWN_TIMEOUT, shard_manager.shutdown_all())
                .await
                .is_err()
            {
                tracing::warn!("Discord shards did not close in time");
            }
            Ok(())
        }
    };

    if let Err(err) = result {
        if matches!(
            err,
            serenity::Error::Gateway(GatewayError::DisallowedGatewayIntents)
        ) {
            tracing::error!(
                "Discord rejected the MESSAGE_CONTENT intent: enable \"Message Content Intent\" \
                 in the Developer Portal (Bot -> Privileged Gateway Intents)"
            );
        }
        return Err(err.into());
    }
    Ok(())
}

/// With a voice manager registered, `Client::start` first fetches the bot
/// user and fails outright if Discord is unreachable. Do that fetch here with
/// backoff, so a short outage at startup doesn't crash-loop the task; a
/// rejected token (or any other 4xx) still fails at once.
async fn wait_for_discord(http: &serenity::http::Http) -> Result<(), AppError> {
    let mut delay = Duration::from_secs(1);
    loop {
        match http.get_current_user().await {
            Ok(_) => return Ok(()),
            Err(err) if is_retryable(&err) => {
                tracing::warn!(%err, retry_in = ?delay, "cannot reach Discord");
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(MAX_CONNECT_BACKOFF);
            }
            Err(err) => return Err(err.into()),
        }
    }
}

/// Transport failures, rate limits and server errors; not 4xx like 401.
fn is_retryable(err: &serenity::Error) -> bool {
    match err {
        serenity::Error::Http(http) => match http.status_code() {
            None => true,
            Some(status) => status.is_server_error() || status.as_u16() == 429,
        },
        _ => false,
    }
}

/// Ctrl-C locally; SIGTERM from `docker stop` and ECS task shutdown.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
                return;
            }
            Err(err) => {
                tracing::warn!(%err, "cannot listen for SIGTERM; only Ctrl-C stops the bot")
            }
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}
