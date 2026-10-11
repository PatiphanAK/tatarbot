//! Slash commands for music. Thin: read the interaction, find the caller's
//! voice channel in the cache, call `MusicService`, render the result.

use serenity::all::{
    CommandInteraction, CommandOptionType, Context, CreateCommand, CreateCommandOption,
    InteractionContext, ResolvedValue,
};

use std::time::Instant;

use super::{presenter, respond};
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
    let received = Instant::now();
    // First, before any voice, stream or lock work: Discord allows 3 s.
    let Some(reply) = respond::defer(ctx, cmd).await else {
        return;
    };
    let defer_ms = received.elapsed().as_millis() as u64;
    let Some(guild_id) = cmd.guild_id else {
        reply.refuse(presenter::MUSIC_GUILD_ONLY.into()).await;
        return;
    };
    let play_ctx = PlayContext {
        guild: GuildId(guild_id.get()),
        user: UserId(cmd.user.id.get()),
        user_channel: user_voice_channel(ctx, guild_id, cmd.user.id).await,
    };
    let refused = |err: crate::application::ports::MusicError| {
        tracing::info!(command = %cmd.data.name, error = %err, "music command refused");
        presenter::music_error(&err)
    };

    let result = match cmd.data.name.as_str() {
        "play" => music
            .play(play_ctx, &string_option(cmd, "query"))
            .await
            .map(|outcome| presenter::play_outcome(&outcome))
            .map_err(refused),
        "radio" => music
            .play_station(play_ctx, &string_option(cmd, "station"))
            .await
            .map(|outcome| presenter::play_outcome(&outcome))
            .map_err(refused),
        "skip" => music
            .skip(play_ctx)
            .await
            .map(|next| presenter::skipped(next.as_ref()))
            .map_err(refused),
        "stop" => music
            .stop(play_ctx)
            .await
            .map(|()| presenter::MUSIC_STOPPED.into())
            .map_err(refused),
        "leave" => music
            .leave(play_ctx)
            .await
            .map(|()| presenter::MUSIC_LEFT.into())
            .map_err(refused),
        "queue" => Ok(presenter::queue(&music.queue(play_ctx.guild).await)),
        other => {
            tracing::warn!(command = other, "unknown music command");
            return;
        }
    };
    tracing::debug!(
        target: "tatarbot::timing",
        command = %cmd.data.name,
        guild = guild_id.get(),
        ok = result.is_ok(),
        discord_to_bot_ms = respond::since_created_ms(cmd),
        defer_ms,
        handled_ms = received.elapsed().as_millis() as u64,
        "music command"
    );
    reply.result(result).await;
}

/// The caller's voice channel. The gateway cache (GUILDS + GUILD_VOICE_STATES
/// intents) normally has it; if it doesn't (e.g. the guild isn't cached yet),
/// ask Discord directly instead of telling the user to join a channel they
/// are already in.
pub(super) async fn user_voice_channel(
    ctx: &Context,
    guild_id: serenity::all::GuildId,
    user_id: serenity::all::UserId,
) -> Option<VoiceChannelId> {
    // The cache guard is not Send: finish with it before awaiting.
    let cached = ctx.cache.guild(guild_id).map(|guild| {
        guild
            .voice_states
            .get(&user_id)
            .and_then(|state| state.channel_id)
    });
    let channel = match cached {
        Some(channel) => channel,
        None => {
            tracing::debug!(guild = %guild_id, "guild not cached; fetching voice state");
            ctx.http
                .get_user_voice_state(guild_id, user_id)
                .await
                .ok()
                .and_then(|state| state.channel_id)
        }
    };
    channel.map(|c| VoiceChannelId(c.get()))
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
