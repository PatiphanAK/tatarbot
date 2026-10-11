use std::sync::Arc;

use serenity::all::{
    Command, Context, CreateAllowedMentions, CreateMessage, EventHandler, Interaction, Message,
    Ready, UserId,
};
use serenity::async_trait;

use std::panic::AssertUnwindSafe;

use futures::FutureExt;

use super::{chat_actions, convert, library, music, presenter, respond};
use crate::application::ports::{ChatService, LibraryService, MusicService, PlayContext};
use crate::domain::ChatRequest;
use crate::domain::music::{GuildId, UserId as MusicUserId};

/// The server library and who may manage it (role name, per server).
pub struct LibraryCommands {
    pub service: Arc<dyn LibraryService>,
    pub upload_role: String,
}

pub struct Handler {
    chat: Arc<dyn ChatService>,
    music: Arc<dyn MusicService>,
    library: Option<LibraryCommands>,
}

impl Handler {
    pub fn new(
        chat: Arc<dyn ChatService>,
        music: Arc<dyn MusicService>,
        library: Option<LibraryCommands>,
    ) -> Self {
        Self {
            chat,
            music,
            library,
        }
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        tracing::info!(user = %ready.user.name, "connected to Discord");
        register_slash_commands(&ctx, &ready, self.music.as_ref(), self.library.is_some()).await;
    }

    /// Discord `MESSAGE_CREATE`. Serenity runs each event in its own task,
    /// so a slow agent call doesn't block other messages.
    async fn message(&self, ctx: Context, msg: Message) {
        let bot_id = ctx.cache.current_user().id;
        if !is_addressed_to_bot(&msg, bot_id) {
            return;
        }
        respond(&ctx, &msg, bot_id, self.chat.as_ref(), self.music.as_ref()).await;
    }

    /// A panic in a command must not leave the caller with "TatarBot is
    /// thinking…" forever: catch it, log it, and reply with an error.
    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        let handled = AssertUnwindSafe(self.dispatch(&ctx, &interaction))
            .catch_unwind()
            .await;
        if let Err(panic) = handled {
            let reason = panic_reason(panic.as_ref());
            if let Interaction::Command(cmd) = &interaction {
                tracing::error!(command = %cmd.data.name, %reason, "command handler panicked");
                respond::report_panic(&ctx, cmd).await;
            } else {
                tracing::error!(%reason, "interaction handler panicked");
            }
        }
    }
}

impl Handler {
    async fn dispatch(&self, ctx: &Context, interaction: &Interaction) {
        match interaction {
            Interaction::Command(cmd) if music::is_music_command(&cmd.data.name) => {
                music::handle(ctx, cmd, self.music.as_ref()).await;
            }
            Interaction::Command(cmd) if library::is_library_command(&cmd.data.name) => {
                if let Some(lib) = &self.library {
                    library::handle(ctx, cmd, lib.service.as_ref(), &lib.upload_role).await;
                }
            }
            // Always answer: an unanswered autocomplete shows "Loading options
            // failed" (e.g. a /play registered with suggestions by a bot that
            // has the library, while this one doesn't).
            Interaction::Autocomplete(cmd) => match &self.library {
                Some(lib) => library::autocomplete(ctx, cmd, lib.service.as_ref()).await,
                None => library::no_suggestions(ctx, cmd).await,
            },
            _ => {}
        }
    }
}

fn panic_reason(panic: &(dyn std::any::Any + Send)) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_owned())
}

/// Respond only to humans who @mention the bot or reply to one of its
/// messages. Bot authors (including ourselves) are ignored to avoid loops.
fn is_addressed_to_bot(msg: &Message, bot_id: UserId) -> bool {
    should_respond(
        msg.author.bot || msg.author.id == bot_id,
        msg.mentions_user_id(bot_id),
        msg.referenced_message.as_ref().map(|m| m.author.id),
        bot_id,
    )
}

fn should_respond(
    author_is_bot: bool,
    mentions_bot: bool,
    replied_to: Option<UserId>,
    bot_id: UserId,
) -> bool {
    !author_is_bot && (mentions_bot || replied_to == Some(bot_id))
}

async fn respond(
    ctx: &Context,
    msg: &Message,
    bot_id: UserId,
    chat: &dyn ChatService,
    music: &dyn MusicService,
) {
    let text = convert::strip_bot_mention(&msg.content, bot_id);
    let conversation_id = convert::conversation_id(msg.guild_id, msg.channel_id, msg.author.id);

    // Show "Bot is typing…" until the reply is sent (stops when dropped).
    let typing = msg.channel_id.start_typing(&ctx.http);

    let messages = match ChatRequest::new(text, conversation_id) {
        Err(_) => vec![presenter::EMPTY_PROMPT.to_owned()],
        Ok(request) => {
            // Music control is offered only in servers (there is no voice in DMs).
            let request = match msg.guild_id {
                Some(_) => request.with_client_tools(chat_actions::offer(music)),
                None => request,
            };
            match chat.chat(request).await {
                Ok(response) => {
                    let results = match msg.guild_id {
                        Some(guild_id) if !response.actions.is_empty() => {
                            let play_ctx = PlayContext {
                                guild: GuildId(guild_id.get()),
                                user: MusicUserId(msg.author.id.get()),
                                user_channel: music::user_voice_channel(
                                    ctx,
                                    guild_id,
                                    msg.author.id,
                                )
                                .await,
                            };
                            chat_actions::execute(music, play_ctx, &response.actions).await
                        }
                        _ => Vec::new(),
                    };
                    chat_actions::compose(&response.message, &results)
                }
                Err(err) => {
                    tracing::warn!(error = ?err, "chat failed");
                    vec![presenter::error_message(&err)]
                }
            }
        }
    };
    let chunks: Vec<String> = messages
        .iter()
        .flat_map(|m| presenter::split_message(m))
        .collect();

    for chunk in chunks {
        // Plain channel message (not a reply); agent output never pings anyone.
        let out = CreateMessage::new()
            .content(chunk)
            .allowed_mentions(CreateAllowedMentions::new());
        if let Err(err) = msg.channel_id.send_message(&ctx.http, out).await {
            // Typically missing Send Messages permission in this channel.
            tracing::warn!(channel = %msg.channel_id, error = %err, "failed to send message");
            break;
        }
    }
    typing.stop();
}

/// Replaces the global commands with the music commands (bulk overwrite, so
/// the old `/chat` disappears too), and removes guild-level leftovers from
/// earlier versions so commands don't show up twice.
async fn register_slash_commands(
    ctx: &Context,
    ready: &Ready,
    music: &dyn MusicService,
    with_library: bool,
) {
    let mut commands = music::commands(music.stations(), with_library);
    if with_library {
        commands.extend(library::commands());
    }
    match Command::set_global_commands(&ctx.http, commands).await {
        Ok(registered) => tracing::info!(count = registered.len(), "registered slash commands"),
        Err(err) => tracing::warn!(%err, "failed to register slash commands"),
    }
    for guild in &ready.guilds {
        if let Ok(cmds) = guild.id.get_commands(&ctx.http).await
            && !cmds.is_empty()
        {
            match guild.id.set_commands(&ctx.http, Vec::new()).await {
                Ok(_) => tracing::info!(guild = %guild.id, "removed guild slash commands"),
                Err(err) => {
                    tracing::warn!(guild = %guild.id, %err, "failed to remove guild slash commands")
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOT: UserId = UserId::new(1);
    const OTHER: UserId = UserId::new(2);

    #[tokio::test]
    async fn panics_are_caught_with_their_message() {
        let caught = AssertUnwindSafe(async { panic!("boom {}", 42) })
            .catch_unwind()
            .await
            .unwrap_err();
        assert_eq!(panic_reason(caught.as_ref()), "boom 42");
        let caught = std::panic::catch_unwind(|| std::panic::panic_any(7u8)).unwrap_err();
        assert_eq!(panic_reason(caught.as_ref()), "unknown panic");
    }

    #[test]
    fn responds_to_mentions() {
        assert!(should_respond(false, true, None, BOT));
    }

    #[test]
    fn responds_to_replies_to_the_bot() {
        assert!(should_respond(false, false, Some(BOT), BOT));
    }

    #[test]
    fn ignores_normal_chat_and_replies_to_others() {
        assert!(!should_respond(false, false, None, BOT));
        assert!(!should_respond(false, false, Some(OTHER), BOT));
    }

    #[test]
    fn ignores_bots_including_itself() {
        assert!(!should_respond(true, true, Some(BOT), BOT));
    }
}
