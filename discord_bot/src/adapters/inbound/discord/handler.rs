use std::sync::Arc;

use serenity::all::{
    Command, Context, CreateAllowedMentions, CreateMessage, EventHandler, Message, Ready, UserId,
};
use serenity::async_trait;

use super::{convert, presenter};
use crate::application::ports::ChatService;
use crate::domain::ChatRequest;

pub struct Handler {
    chat: Arc<dyn ChatService>,
}

impl Handler {
    pub fn new(chat: Arc<dyn ChatService>) -> Self {
        Self { chat }
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        tracing::info!(user = %ready.user.name, "connected to Discord");
        remove_stale_slash_commands(&ctx, &ready).await;
    }

    /// Discord `MESSAGE_CREATE`. Serenity runs each event in its own task,
    /// so a slow agent call doesn't block other messages.
    async fn message(&self, ctx: Context, msg: Message) {
        let bot_id = ctx.cache.current_user().id;
        if !is_addressed_to_bot(&msg, bot_id) {
            return;
        }
        respond(&ctx, &msg, bot_id, self.chat.as_ref()).await;
    }
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

async fn respond(ctx: &Context, msg: &Message, bot_id: UserId, chat: &dyn ChatService) {
    let text = convert::strip_bot_mention(&msg.content, bot_id);
    let conversation_id = convert::conversation_id(msg.guild_id, msg.channel_id, msg.author.id);

    // Show "Bot is typing…" until the reply is sent (stops when dropped).
    let typing = msg.channel_id.start_typing(&ctx.http);

    let chunks = match ChatRequest::new(text, conversation_id) {
        Err(_) => vec![presenter::EMPTY_PROMPT.to_owned()],
        Ok(request) => match chat.chat(request).await {
            Ok(response) => presenter::split_message(&response.message),
            Err(err) => {
                tracing::warn!(error = ?err, "chat failed");
                vec![presenter::error_message(&err)]
            }
        },
    };

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

/// Earlier versions registered `/chat`; delete it so users don't see a
/// command that no longer works. No-op once cleaned up.
async fn remove_stale_slash_commands(ctx: &Context, ready: &Ready) {
    match Command::get_global_commands(&ctx.http).await {
        Ok(cmds) if !cmds.is_empty() => {
            match Command::set_global_commands(&ctx.http, Vec::new()).await {
                Ok(_) => tracing::info!(count = cmds.len(), "removed global slash commands"),
                Err(err) => tracing::warn!(%err, "failed to remove global slash commands"),
            }
        }
        Ok(_) => {}
        Err(err) => tracing::warn!(%err, "failed to list global slash commands"),
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
