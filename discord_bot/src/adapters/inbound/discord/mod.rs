//! Inbound adapter: turns Discord messages addressed to the bot into calls
//! on the `ChatService` port and renders results back as Discord messages.
//!
//! Everything Discord-specific (IDs, intents, message limits, mention
//! handling, error wording) stays inside this module.

mod convert;
mod handler;
mod presenter;

use std::sync::Arc;
use std::time::Duration;

use serenity::all::{Client, GatewayError, GatewayIntents};

use crate::application::ports::ChatService;
use crate::config::DiscordConfig;
use crate::error::AppError;

/// Well below ECS's stopTimeout and Docker's 10 s default before SIGKILL.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

pub async fn run(config: DiscordConfig, chat: Arc<dyn ChatService>) -> Result<(), AppError> {
    // MESSAGE_CONTENT (privileged) is required to read replies to the bot
    // that don't ping it; enable it under Bot -> Privileged Gateway Intents.
    let intents = GatewayIntents::GUILD_MESSAGES
        | GatewayIntents::DIRECT_MESSAGES
        | GatewayIntents::MESSAGE_CONTENT;

    let mut client = Client::builder(&config.token, intents)
        .event_handler(handler::Handler::new(chat))
        .await?;

    let shard_manager = client.shard_manager.clone();
    let result = tokio::select! {
        result = client.start() => result,
        () = shutdown_signal() => {
            tracing::info!("shutting down");
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
