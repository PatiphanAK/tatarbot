//! Replying to slash commands within Discord's 3-second window.
//!
//! Every command is deferred before it does anything else (joining voice,
//! opening a stream, a database call or even waiting for a lock can take
//! longer than 3 s). The deferred reply is public; a refusal deletes it and
//! sends an ephemeral follow-up instead, so errors stay visible only to the
//! caller.

use serenity::all::{
    CommandInteraction, Context, CreateAllowedMentions, CreateInteractionResponseFollowup,
    EditInteractionResponse,
};

use super::presenter;

pub struct Deferred<'a> {
    ctx: &'a Context,
    cmd: &'a CommandInteraction,
}

/// Acknowledges the command ("TatarBot is thinking…"). `None` if Discord
/// refused, e.g. the interaction already expired or another process with the
/// same token answered it first.
pub async fn defer<'a>(ctx: &'a Context, cmd: &'a CommandInteraction) -> Option<Deferred<'a>> {
    match cmd.defer(&ctx.http).await {
        Ok(()) => Some(Deferred { ctx, cmd }),
        Err(err) => {
            tracing::warn!(
                command = %cmd.data.name,
                %err,
                "failed to defer interaction (expired, or another bot process answered it)"
            );
            None
        }
    }
}

impl Deferred<'_> {
    /// Public result.
    pub async fn ok(self, text: String) {
        let edit = EditInteractionResponse::new()
            .content(text)
            .allowed_mentions(CreateAllowedMentions::new());
        if let Err(err) = self.cmd.edit_response(&self.ctx.http, edit).await {
            tracing::warn!(command = %self.cmd.data.name, %err, "failed to send reply");
        }
    }

    /// Refusal, shown only to the caller.
    pub async fn refuse(self, text: String) {
        if let Err(err) = self.cmd.delete_response(&self.ctx.http).await {
            tracing::debug!(%err, "could not delete the deferred reply");
        }
        send_ephemeral_followup(self.ctx, self.cmd, text).await;
    }

    pub async fn result(self, result: Result<String, String>) {
        match result {
            Ok(text) => self.ok(text).await,
            Err(text) => self.refuse(text).await,
        }
    }
}

async fn send_ephemeral_followup(ctx: &Context, cmd: &CommandInteraction, text: String) {
    let followup = CreateInteractionResponseFollowup::new()
        .content(text)
        .ephemeral(true)
        .allowed_mentions(CreateAllowedMentions::new());
    if let Err(err) = cmd.create_followup(&ctx.http, followup).await {
        tracing::warn!(command = %cmd.data.name, %err, "failed to send reply");
    }
}

/// After a handler panicked: tell the caller instead of leaving "TatarBot is
/// thinking…" forever. Works whether or not the command was deferred.
pub async fn report_panic(ctx: &Context, cmd: &CommandInteraction) {
    let text = presenter::command_failed();
    let created = cmd
        .create_response(
            &ctx.http,
            serenity::all::CreateInteractionResponse::Message(
                serenity::all::CreateInteractionResponseMessage::new()
                    .content(text.clone())
                    .ephemeral(true),
            ),
        )
        .await;
    if created.is_err() {
        // Already deferred: replace the placeholder.
        let edit = EditInteractionResponse::new().content(text);
        if let Err(err) = cmd.edit_response(&ctx.http, edit).await {
            tracing::warn!(%err, "could not report the failure to the caller");
        }
    }
}
