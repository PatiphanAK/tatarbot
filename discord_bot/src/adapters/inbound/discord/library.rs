//! Slash commands for the server library (`/upload`, `/library`, `/delete`)
//! and title autocomplete for `/play` and `/delete`. Thin, like `music.rs`:
//! permissions are read from the interaction, everything else is
//! `LibraryService`.

use serenity::all::{
    CommandInteraction, CommandOptionType, Context, CreateAutocompleteResponse, CreateCommand,
    CreateCommandOption, CreateInteractionResponse, InteractionContext, ResolvedValue,
};

use super::{presenter, respond};
use crate::application::ports::{Attachment, LibraryError, LibraryService, UploadRequest};
use crate::domain::library::{LibraryTrackId, MAX_TITLE_CHARS};
use crate::domain::music::{GuildId, UserId};

const AUTOCOMPLETE_CHOICES: usize = 25;
/// Leaves ~1 s of Discord's 3 s for the round trip.
const AUTOCOMPLETE_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);

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
    // First: downloads, transcodes and database calls can exceed Discord's 3 s.
    let Some(reply) = respond::defer(ctx, cmd).await else {
        return;
    };
    let Some(guild_id) = cmd.guild_id else {
        reply.refuse(presenter::MUSIC_GUILD_ONLY.into()).await;
        return;
    };
    let guild = GuildId(guild_id.get());
    let user = UserId(cmd.user.id.get());
    let manager = can_manage(ctx, cmd, upload_role);
    let refused = |err: LibraryError| {
        tracing::info!(command = %cmd.data.name, error = %err, "library command refused");
        presenter::library_error(&err)
    };

    let result = match cmd.data.name.as_str() {
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
            match attachment {
                None => Err(presenter::library_error(&LibraryError::NotAudio)),
                Some(attachment) => library
                    .upload(UploadRequest {
                        guild,
                        user,
                        can_upload: manager,
                        attachment,
                        title,
                    })
                    .await
                    .map(|outcome| presenter::uploaded(&outcome))
                    .map_err(refused),
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
            library
                .list(guild, page)
                .await
                .map(|page| presenter::library_page(&page))
                .map_err(refused)
        }
        "delete" => {
            let id = cmd.data.options().into_iter().find_map(|o| match o.value {
                ResolvedValue::String(s) => s.trim().parse::<i64>().ok(),
                _ => None,
            });
            match id {
                None => Err(format!(
                    "{} Pick one from the list.",
                    presenter::library_error(&LibraryError::NotFound)
                )),
                Some(id) => library
                    .delete(guild, user, manager, LibraryTrackId(id))
                    .await
                    .map(|track| presenter::deleted(&track))
                    .map_err(refused),
            }
        }
        other => {
            tracing::warn!(command = other, "unknown library command");
            return;
        }
    };
    reply.result(result).await;
}

/// Title suggestions for `/play query` (value: the title) and `/delete track`
/// (value: the track id). An unavailable library suggests nothing.
pub async fn autocomplete(ctx: &Context, cmd: &CommandInteraction, library: &dyn LibraryService) {
    let (Some(guild_id), Some(focused)) = (cmd.guild_id, cmd.data.autocomplete()) else {
        no_suggestions(ctx, cmd).await;
        return;
    };
    let received = std::time::Instant::now();
    let by_id = cmd.data.name == "delete";
    let typed = focused.value;
    let tracks = if !by_id && typed.contains("://") {
        Vec::new() // a link: nothing to suggest
    } else {
        // Autocomplete can't be deferred: answer within Discord's 3 s or not
        // at all. A slow database means no suggestions, not an error.
        let search = library.search(GuildId(guild_id.get()), typed, AUTOCOMPLETE_CHOICES);
        match tokio::time::timeout(AUTOCOMPLETE_BUDGET, search).await {
            Ok(Ok(tracks)) => tracks,
            Ok(Err(_)) => Vec::new(),
            Err(_) => {
                tracing::warn!(budget = ?AUTOCOMPLETE_BUDGET, "autocomplete search too slow");
                Vec::new()
            }
        }
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
    let sent = cmd
        .create_response(&ctx.http, CreateInteractionResponse::Autocomplete(response))
        .await;
    tracing::debug!(
        target: "tatarbot::timing",
        command = %cmd.data.name,
        chars = typed.chars().count(),
        choices = tracks.len(),
        ok = sent.is_ok(),
        discord_to_bot_ms = super::music::since_created_ms(cmd),
        replied_ms = received.elapsed().as_millis() as u64,
        "autocomplete"
    );
    if let Err(err) = sent {
        tracing::debug!(%err, "autocomplete reply failed");
    }
}

/// An empty answer, so Discord shows "no options" instead of an error.
pub async fn no_suggestions(ctx: &Context, cmd: &CommandInteraction) {
    let empty = CreateInteractionResponse::Autocomplete(CreateAutocompleteResponse::new());
    if let Err(err) = cmd.create_response(&ctx.http, empty).await {
        tracing::debug!(%err, "autocomplete reply failed");
    }
}
