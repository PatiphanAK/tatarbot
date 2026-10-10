//! Slash commands for the server library (`/upload`, `/library`, `/delete`)
//! and title autocomplete for `/play` and `/delete`. Thin, like `music.rs`:
//! permissions are read from the interaction, everything else is
//! `LibraryService`.

use serenity::all::{
    CommandInteraction, CommandOptionType, Context, CreateAllowedMentions,
    CreateAutocompleteResponse, CreateCommand, CreateCommandOption, CreateInteractionResponse,
    CreateInteractionResponseMessage, EditInteractionResponse, InteractionContext, ResolvedValue,
};

use super::presenter;
use crate::application::ports::{Attachment, LibraryService, UploadRequest};
use crate::domain::library::{LibraryTrackId, MAX_TITLE_CHARS};
use crate::domain::music::{GuildId, UserId};

const AUTOCOMPLETE_CHOICES: usize = 25;

pub fn commands() -> Vec<CreateCommand> {
    let guild_only = || vec![InteractionContext::Guild];
    vec![
        CreateCommand::new("upload")
            .description("Add an audio file to this server's library")
            .contexts(guild_only())
            .add_option(
                CreateCommandOption::new(CommandOptionType::Attachment, "file", "Audio file")
                    .required(true),
            )
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::String,
                    "title",
                    "Title (default: file name)",
                )
                .max_length(MAX_TITLE_CHARS as u16),
            ),
        CreateCommand::new("library")
            .description("List this server's library")
            .contexts(guild_only())
            .add_option(
                CreateCommandOption::new(CommandOptionType::Integer, "page", "Page")
                    .min_int_value(1),
            ),
        CreateCommand::new("delete")
            .description("Remove a track from this server's library")
            .contexts(guild_only())
            .add_option(
                CreateCommandOption::new(CommandOptionType::String, "track", "Track")
                    .required(true)
                    .set_autocomplete(true),
            ),
    ]
}

pub fn is_library_command(name: &str) -> bool {
    matches!(name, "upload" | "library" | "delete")
}

/// Who may upload or delete others' tracks: members with the configured role
/// (by name, per server), or with Manage Server / Administrator.
fn can_manage(ctx: &Context, cmd: &CommandInteraction, upload_role: &str) -> bool {
    let Some(member) = &cmd.member else {
        return false;
    };
    if member
        .permissions
        .is_some_and(|p| p.administrator() || p.manage_guild())
    {
        return true;
    }
    let Some(guild_id) = cmd.guild_id else {
        return false;
    };
    ctx.cache.guild(guild_id).is_some_and(|guild| {
        member.roles.iter().any(|role| {
            guild
                .roles
                .get(role)
                .is_some_and(|r| r.name.eq_ignore_ascii_case(upload_role))
        })
    })
}

pub async fn handle(
    ctx: &Context,
    cmd: &CommandInteraction,
    library: &dyn LibraryService,
    upload_role: &str,
) {
    let Some(guild_id) = cmd.guild_id else {
        reply(ctx, cmd, presenter::MUSIC_GUILD_ONLY.into(), true).await;
        return;
    };
    let guild = GuildId(guild_id.get());
    let user = UserId(cmd.user.id.get());
    let manager = can_manage(ctx, cmd, upload_role);

    match cmd.data.name.as_str() {
        "upload" => {
            let mut attachment = None;
            let mut title = None;
            for option in cmd.data.options() {
                match (option.name, option.value) {
                    ("file", ResolvedValue::Attachment(a)) => {
                        attachment = Some(Attachment {
                            url: a.url.clone(),
                            filename: a.filename.clone(),
                            declared_size: u64::from(a.size),
                        });
                    }
                    ("title", ResolvedValue::String(s)) => title = Some(s.to_owned()),
                    _ => {}
                }
            }
            let Some(attachment) = attachment else {
                return;
            };
            // Download, probe and transcode take longer than Discord's 3 s.
            if let Err(err) = cmd.defer(&ctx.http).await {
                tracing::warn!(%err, "failed to defer interaction");
                return;
            }
            let result = library
                .upload(UploadRequest {
                    guild,
                    user,
                    can_upload: manager,
                    attachment,
                    title,
                })
                .await;
            let text = match result {
                Ok(outcome) => presenter::uploaded(&outcome),
                Err(err) => {
                    tracing::info!(error = %err, "upload refused");
                    presenter::library_error(&err)
                }
            };
            let edit = EditInteractionResponse::new()
                .content(text)
                .allowed_mentions(CreateAllowedMentions::new());
            if let Err(err) = cmd.edit_response(&ctx.http, edit).await {
                tracing::warn!(%err, "failed to send upload reply");
            }
        }
        "library" => {
            let page = cmd
                .data
                .options()
                .into_iter()
                .find_map(|o| match o.value {
                    ResolvedValue::Integer(n) => usize::try_from(n).ok(),
                    _ => None,
                })
                .unwrap_or(1);
            match library.list(guild, page).await {
                Ok(page) => reply(ctx, cmd, presenter::library_page(&page), false).await,
                Err(err) => reply(ctx, cmd, presenter::library_error(&err), true).await,
            }
        }
        "delete" => {
            let id = cmd.data.options().into_iter().find_map(|o| match o.value {
                ResolvedValue::String(s) => s.trim().parse::<i64>().ok(),
                _ => None,
            });
            let Some(id) = id else {
                let text =
                    presenter::library_error(&crate::application::ports::LibraryError::NotFound);
                reply(ctx, cmd, format!("{text} Pick one from the list."), true).await;
                return;
            };
            match library
                .delete(guild, user, manager, LibraryTrackId(id))
                .await
            {
                Ok(track) => reply(ctx, cmd, presenter::deleted(&track), false).await,
                Err(err) => reply(ctx, cmd, presenter::library_error(&err), true).await,
            }
        }
        other => tracing::warn!(command = other, "unknown library command"),
    }
}

/// Title suggestions for `/play query` (value: the title) and `/delete track`
/// (value: the track id). An unavailable library suggests nothing.
pub async fn autocomplete(ctx: &Context, cmd: &CommandInteraction, library: &dyn LibraryService) {
    let (Some(guild_id), Some(focused)) = (cmd.guild_id, cmd.data.autocomplete()) else {
        return;
    };
    let by_id = cmd.data.name == "delete";
    let typed = focused.value;
    let tracks = if !by_id && typed.contains("://") {
        Vec::new() // a link: nothing to suggest
    } else {
        library
            .search(GuildId(guild_id.get()), typed, AUTOCOMPLETE_CHOICES)
            .await
            .unwrap_or_default()
    };
    let response = tracks
        .iter()
        .fold(CreateAutocompleteResponse::new(), |r, track| {
            let value = if by_id {
                track.id.0.to_string()
            } else {
                track.title.clone()
            };
            r.add_string_choice(presenter::library_choice(track), value)
        });
    if let Err(err) = cmd
        .create_response(&ctx.http, CreateInteractionResponse::Autocomplete(response))
        .await
    {
        tracing::debug!(%err, "autocomplete reply failed");
    }
}

async fn reply(ctx: &Context, cmd: &CommandInteraction, text: String, ephemeral: bool) {
    let message = CreateInteractionResponseMessage::new()
        .content(text)
        .ephemeral(ephemeral)
        .allowed_mentions(CreateAllowedMentions::new());
    if let Err(err) = cmd
        .create_response(&ctx.http, CreateInteractionResponse::Message(message))
        .await
    {
        tracing::warn!(%err, "failed to send library reply");
    }
}
