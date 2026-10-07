//! Inbound adapter: turns Discord messages addressed to the bot into calls
//! on the `ChatService` port and renders results back as Discord messages.
//!
//! Everything Discord-specific (IDs, intents, message limits, mention
//! handling, error wording) stays inside this module.

mod convert;
mod handler;
mod presenter;

use std::sync::Arc;

use serenity::all::{Client, GatewayError, GatewayIntents};

use crate::application::ports::ChatService;
use crate::config::DiscordConfig;
use crate::error::AppError;

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
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            tracing::info!("shutting down");
            shard_manager.shutdown_all().await;
        }
    });

    if let Err(err) = client.start().await {
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
