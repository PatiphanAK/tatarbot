use poise::serenity_prelude as serenity;

use super::{Context, Data, Error, convert, presenter};
use crate::domain::ChatRequest;

pub fn all() -> Vec<poise::Command<Data, Error>> {
    vec![chat()]
}

/// Chat with TatarBot
#[poise::command(slash_command)]
async fn chat(
    ctx: Context<'_>,
    #[description = "What do you want to say?"]
    #[max_length = 4000]
    message: String,
) -> Result<(), Error> {
    let conversation_id =
        convert::conversation_id(ctx.guild_id(), ctx.channel_id(), ctx.author().id);
    let Ok(request) = ChatRequest::new(message, conversation_id) else {
        ctx.send(reply(presenter::EMPTY_PROMPT).ephemeral(true))
            .await?;
        return Ok(());
    };

    // Agent calls can be slow; acknowledge within Discord's 3s window.
    ctx.defer().await?;

    match ctx.data().chat.chat(request).await {
        Ok(response) => {
            for chunk in presenter::split_message(&response.message) {
                ctx.send(reply(&chunk)).await?;
            }
        }
        Err(err) => {
            tracing::warn!(error = ?err, "chat failed");
            ctx.send(reply(&presenter::error_message(&err))).await?;
        }
    }
    Ok(())
}

/// Agent output must never ping @everyone, roles or users.
fn reply(content: &str) -> poise::CreateReply {
    poise::CreateReply::default()
        .content(content)
        .allowed_mentions(serenity::CreateAllowedMentions::new())
}
