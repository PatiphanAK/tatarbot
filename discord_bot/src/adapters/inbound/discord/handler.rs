use std::sync::Arc;

use poise::serenity_prelude as serenity;

use super::{Data, Error, convert, presenter};
use crate::application::ports::ChatService;
use crate::domain::ChatRequest;

pub async fn on_event(
    ctx: &serenity::Context,
    event: &serenity::FullEvent,
    _framework: poise::FrameworkContext<'_, Data, Error>,
    data: &Data,
) -> Result<(), Error> {
    if let serenity::FullEvent::Message { new_message } = event
        && data.respond_to_mentions
        && should_respond(ctx, new_message)
    {
        // Agent calls can take a while; don't hold up the event loop.
        tokio::spawn(respond(
            ctx.clone(),
            new_message.clone(),
            Arc::clone(&data.chat),
        ));
    }
    Ok(())
}

/// Respond to DMs and to guild messages that mention the bot.
fn should_respond(ctx: &serenity::Context, msg: &serenity::Message) -> bool {
    if msg.author.bot {
        return false;
    }
    msg.guild_id.is_none() || msg.mentions_user_id(ctx.cache.current_user().id)
}

async fn respond(ctx: serenity::Context, msg: serenity::Message, chat: Arc<dyn ChatService>) {
    let bot_id = ctx.cache.current_user().id;
    let text = convert::strip_bot_mention(&msg.content, bot_id);
    let conversation_id = convert::conversation_id(msg.guild_id, msg.channel_id, msg.author.id);

    let chunks = match ChatRequest::new(text, conversation_id) {
        Err(_) => vec![presenter::EMPTY_PROMPT.to_owned()],
        Ok(request) => {
            let _typing = msg.channel_id.start_typing(&ctx.http);
            match chat.chat(request).await {
                Ok(response) => presenter::split_message(&response.message),
                Err(err) => {
                    tracing::warn!(error = ?err, "chat failed");
                    vec![presenter::error_message(&err)]
                }
            }
        }
    };

    for (i, chunk) in chunks.into_iter().enumerate() {
        let mut out = serenity::CreateMessage::new()
            .content(chunk)
            .allowed_mentions(serenity::CreateAllowedMentions::new().replied_user(true));
        if i == 0 {
            out = out.reference_message(&msg);
        }
        if let Err(err) = msg.channel_id.send_message(&ctx.http, out).await {
            // Typically missing SEND_MESSAGES / READ_MESSAGE_HISTORY.
            tracing::warn!(channel = %msg.channel_id, error = %err, "failed to send reply");
            break;
        }
    }
}
