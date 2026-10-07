//! Inbound adapter: translates Discord interactions into calls on the
//! `ChatService` port and renders results back as Discord messages.
//!
//! Everything Discord-specific (IDs, intents, message limits, mention
//! handling, error wording) stays inside this module.

mod commands;
mod convert;
mod handler;
mod presenter;

use std::sync::Arc;

use poise::serenity_prelude as serenity;

use crate::application::ports::ChatService;
use crate::config::DiscordConfig;
use crate::error::AppError;

/// State shared with every command and event handler.
pub struct Data {
    chat: Arc<dyn ChatService>,
    respond_to_mentions: bool,
}

type Error = Box<dyn std::error::Error + Send + Sync>;
type Context<'a> = poise::Context<'a, Data, Error>;

pub async fn run(config: DiscordConfig, chat: Arc<dyn ChatService>) -> Result<(), AppError> {
    // Message content of DMs and of messages that mention the bot is
    // delivered without the privileged MESSAGE_CONTENT intent.
    let intents = serenity::GatewayIntents::non_privileged();
    let DiscordConfig {
        token,
        dev_guild_id,
        respond_to_mentions,
    } = config;

    let framework = poise::Framework::builder()
        .options(poise::FrameworkOptions {
            commands: commands::all(),
            event_handler: |ctx, event, framework, data| {
                Box::pin(handler::on_event(ctx, event, framework, data))
            },
            on_error: |error| Box::pin(on_error(error)),
            ..Default::default()
        })
        .setup(move |ctx, ready, framework| {
            Box::pin(async move {
                let commands = &framework.options().commands;
                match dev_guild_id {
                    Some(id) => {
                        let guild = serenity::GuildId::new(id);
                        poise::builtins::register_in_guild(ctx, commands, guild).await?;
                        tracing::info!(%guild, "registered slash commands in dev guild");
                    }
                    None => {
                        poise::builtins::register_globally(ctx, commands).await?;
                        tracing::info!("registered slash commands globally");
                    }
                }
                tracing::info!(user = %ready.user.name, "connected to Discord");
                Ok(Data {
                    chat,
                    respond_to_mentions,
                })
            })
        })
        .build();

    let mut client = serenity::ClientBuilder::new(&token, intents)
        .framework(framework)
        .await?;

    let shard_manager = client.shard_manager.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            tracing::info!("shutting down");
            shard_manager.shutdown_all().await;
        }
    });

    client.start().await?;
    Ok(())
}

async fn on_error(error: poise::FrameworkError<'_, Data, Error>) {
    match error {
        poise::FrameworkError::Setup { error, .. } => {
            tracing::error!(%error, "failed to set up Discord framework");
        }
        poise::FrameworkError::Command { error, ctx, .. } => {
            tracing::error!(command = %ctx.command().qualified_name, %error, "command failed");
            let reply = poise::CreateReply::default()
                .content(presenter::GENERIC_FAILURE)
                .ephemeral(true);
            if let Err(e) = ctx.send(reply).await {
                tracing::warn!(error = %e, "failed to report command error to user");
            }
        }
        other => {
            if let Err(e) = poise::builtins::on_error(other).await {
                tracing::error!(error = %e, "failed to handle framework error");
            }
        }
    }
}
