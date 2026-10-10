//! Slash commands for music. Thin: read the interaction, find the caller's
//! voice channel in the cache, call `MusicService`, render the result.

use serenity::all::{
    CommandInteraction, CommandOptionType, Context, CreateAllowedMentions, CreateCommand,
    CreateCommandOption, CreateInteractionResponse, CreateInteractionResponseMessage,
    EditInteractionResponse, InteractionContext, ResolvedValue,
};

use super::presenter;
use crate::application::ports::{MusicService, PlayContext};
use crate::domain::music::{GuildId, StationList, UserId, VoiceChannelId};

const MAX_QUERY_LENGTH: u16 = 500;
const CHOICE_NAME_LIMIT: usize = 100;

/// Every music command, registered globally on startup. With a library,
/// `/play` also takes titles and suggests them as you type.
pub fn commands(stations: &StationList, library: bool) -> Vec<CreateCommand> {
    let guild_only = || vec![InteractionContext::Guild];
    let radio_station = stations.iter().fold(
        CreateCommandOption::new(CommandOptionType::String, "station", "Station to play")
            .required(true),
        |option, s| {
            let name: String = format!("{} · {}", s.name, s.title)
                .chars()
                .take(CHOICE_NAME_LIMIT)
                .collect();
            option.add_string_choice(name, &s.name)
        },
    );
    vec![
        CreateCommand::new("play")
            .description(if library {
                "Play a track from this server's library, or a direct audio link"
            } else {
                "Play a direct audio link (mp3, ogg, aac, flac, wav, or a stream URL)"
            })
            .contexts(guild_only())
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::String,
                    "query",
                    if library {
                        "Title or audio link"
                    } else {
                        "Audio link"
                    },
                )
                .required(true)
                .max_length(MAX_QUERY_LENGTH)
                .set_autocomplete(library),
            ),
        CreateCommand::new("radio")
            .description("Play a radio station (replaces the queue)")
            .contexts(guild_only())
            .add_option(radio_station),
        CreateCommand::new("skip")
            .description("Skip the current track")
            .contexts(guild_only()),
        CreateCommand::new("stop")
            .description("Stop playback and clear the queue")
            .contexts(guild_only()),
        CreateCommand::new("queue")
            .description("Show what is playing and what's next")
            .contexts(guild_only()),
        CreateCommand::new("leave")
            .description("Stop playback and leave the voice channel")
            .contexts(guild_only()),
    ]
}

pub fn is_music_command(name: &str) -> bool {
    matches!(name, "play" | "radio" | "skip" | "stop" | "queue" | "leave")
}

pub async fn handle(ctx: &Context, cmd: &CommandInteraction, music: &dyn MusicService) {
    let Some(guild_id) = cmd.guild_id else {
        reply(ctx, cmd, presenter::MUSIC_GUILD_ONLY.into(), true).await;
        return;
    };
    // The cache guard is not Send: read the voice state before any await.
    let user_channel = ctx.cache.guild(guild_id).and_then(|guild| {
        guild
            .voice_states
            .get(&cmd.user.id)
            .and_then(|state| state.channel_id)
            .map(|channel| VoiceChannelId(channel.get()))
    });
    let play_ctx = PlayContext {
        guild: GuildId(guild_id.get()),
        user: UserId(cmd.user.id.get()),
        user_channel,
    };

    match cmd.data.name.as_str() {
        name @ ("play" | "radio") => {
            let arg = string_option(cmd, if name == "play" { "query" } else { "station" });
            // Opening a voice connection can exceed Discord's 3 s reply window.
            if let Err(err) = cmd.defer(&ctx.http).await {
                tracing::warn!(%err, "failed to defer interaction");
                return;
            }
            let result = if name == "play" {
                music.play(play_ctx, &arg).await
            } else {
                music.play_station(play_ctx, &arg).await
            };
            let text = match result {
                Ok(outcome) => presenter::play_outcome(&outcome),
                Err(err) => {
                    tracing::info!(command = name, error = %err, "music command refused");
                    presenter::music_error(&err)
                }
            };
            let edit = EditInteractionResponse::new()
                .content(text)
                .allowed_mentions(CreateAllowedMentions::new());
            if let Err(err) = cmd.edit_response(&ctx.http, edit).await {
                tracing::warn!(%err, "failed to send music reply");
            }
        }
        "skip" => match music.skip(play_ctx).await {
            Ok(next) => reply(ctx, cmd, presenter::skipped(next.as_ref()), false).await,
            Err(err) => reply(ctx, cmd, presenter::music_error(&err), true).await,
        },
        "stop" => match music.stop(play_ctx).await {
            Ok(()) => reply(ctx, cmd, presenter::MUSIC_STOPPED.into(), false).await,
            Err(err) => reply(ctx, cmd, presenter::music_error(&err), true).await,
        },
        "leave" => match music.leave(play_ctx).await {
            Ok(()) => reply(ctx, cmd, presenter::MUSIC_LEFT.into(), false).await,
            Err(err) => reply(ctx, cmd, presenter::music_error(&err), true).await,
        },
        "queue" => {
            let queue = music.queue(play_ctx.guild).await;
            reply(ctx, cmd, presenter::queue(&queue), false).await;
        }
        other => tracing::warn!(command = other, "unknown music command"),
    }
}

fn string_option(cmd: &CommandInteraction, name: &str) -> String {
    cmd.data
        .options()
        .into_iter()
        .find(|o| o.name == name)
        .and_then(|o| match o.value {
            ResolvedValue::String(s) => Some(s.to_owned()),
            _ => None,
        })
        .unwrap_or_default()
}

/// Errors are ephemeral (only the caller sees them); results are public.
async fn reply(ctx: &Context, cmd: &CommandInteraction, text: String, ephemeral: bool) {
    let message = CreateInteractionResponseMessage::new()
        .content(text)
        .ephemeral(ephemeral)
        .allowed_mentions(CreateAllowedMentions::new());
    if let Err(err) = cmd
        .create_response(&ctx.http, CreateInteractionResponse::Message(message))
        .await
    {
        tracing::warn!(%err, "failed to send music reply");
    }
}
