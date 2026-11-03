//! Delivers upload outcomes from the transactional outbox (ADR 0004 §3). The bot is the only
//! sender. Each send is safe to repeat: editing the original reply is idempotent, and a
//! channel message carries a nonce with `enforce_nonce`, which Discord deduplicates for a few
//! minutes. Claims expire after 60 s, well inside that window.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serenity::all::{
    ChannelId, CreateAllowedMentions, CreateMessage, Http, Nonce, UserId as SerenityUserId,
};
use sqlx::PgPool;

use super::presenter;
use crate::ingest::outbox::{self, Claimed, Via};

const POLL: Duration = Duration::from_secs(3);
const HOUSEKEEPING_EVERY: u32 = 100; // iterations (~5 min)

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendError {
    /// The interaction token or original reply is no longer usable: fall back to the channel.
    TokenUnusable(String),
    /// Retrying can't help (channel gone, missing permission).
    Permanent(String),
    /// Rate limit, Discord 5xx, network: retry later.
    Transient(String),
}

#[async_trait]
pub trait DiscordSender: Send + Sync {
    async fn edit_original(&self, token: &str, content: &str) -> Result<(), SendError>;
    /// Posts `content` mentioning `user`; returns the message id.
    async fn post_channel(
        &self,
        channel: u64,
        user: u64,
        content: &str,
        nonce: &str,
    ) -> Result<u64, SendError>;
}

pub struct SerenitySender(pub Arc<Http>);

/// HTTP status of a failed Discord call; 0 for network and other errors.
fn status_of(err: &serenity::Error) -> u16 {
    match err {
        serenity::Error::Http(serenity::http::HttpError::UnsuccessfulRequest(resp)) => {
            resp.status_code.as_u16()
        }
        _ => 0,
    }
}

#[async_trait]
impl DiscordSender for SerenitySender {
    async fn edit_original(&self, token: &str, content: &str) -> Result<(), SendError> {
        let body = serde_json::json!({ "content": content, "allowed_mentions": { "parse": [] } });
        match self
            .0
            .edit_original_interaction_response(token, &body, vec![])
            .await
        {
            Ok(_) => Ok(()),
            Err(err) => match status_of(&err) {
                429 | 500..=599 | 0 => Err(SendError::Transient(err.to_string())),
                // 401/404 (expired token, deleted reply) and other refusals.
                _ => Err(SendError::TokenUnusable(err.to_string())),
            },
        }
    }

    async fn post_channel(
        &self,
        channel: u64,
        user: u64,
        content: &str,
        nonce: &str,
    ) -> Result<u64, SendError> {
        let message = CreateMessage::new()
            .content(format!("<@{user}> {content}"))
            .nonce(Nonce::String(nonce.to_owned()))
            .enforce_nonce(true)
            .allowed_mentions(CreateAllowedMentions::new().users([SerenityUserId::new(user)]));
        match ChannelId::new(channel).send_message(&self.0, message).await {
            Ok(m) => Ok(m.id.get()),
            Err(err) => match status_of(&err) {
                429 | 500..=599 | 0 => Err(SendError::Transient(err.to_string())),
                // 403/404: missing access or permission, unknown channel.
                _ => Err(SendError::Permanent(err.to_string())),
            },
        }
    }
}

/// Claims and delivers one notification; `Ok(false)` when nothing was due.
pub async fn deliver_one(pool: &PgPool, sender: &dyn DiscordSender) -> Result<bool, sqlx::Error> {
    let Some(claimed) = outbox::claim(pool).await? else {
        return Ok(false);
    };
    let content = presenter::upload_notice(&claimed.notice);
    match send(sender, &claimed, &content).await {
        Ok((via, message)) => outbox::delivered(pool, &claimed, via, message).await?,
        Err(SendError::Permanent(e)) => outbox::undeliverable(pool, &claimed, &e).await?,
        Err(SendError::Transient(e) | SendError::TokenUnusable(e)) => {
            outbox::retry_later(pool, &claimed, &e).await?;
        }
    }
    Ok(true)
}

async fn send(
    sender: &dyn DiscordSender,
    c: &Claimed,
    content: &str,
) -> Result<(Via, Option<u64>), SendError> {
    if let Some(token) = &c.interaction_token {
        match sender.edit_original(token, content).await {
            Ok(()) => return Ok((Via::Interaction, None)),
            Err(SendError::TokenUnusable(reason)) => {
                tracing::info!(
                    notification = c.id,
                    reason,
                    "original reply unusable; posting in the channel"
                );
            }
            Err(other) => return Err(other),
        }
    }
    let id = sender
        .post_channel(c.channel_id, c.user_id, content, &c.nonce)
        .await?;
    Ok((Via::Channel, Some(id)))
}

/// Background loop: drains due notifications, then waits `POLL`.
pub async fn run(pool: PgPool, sender: Arc<dyn DiscordSender>) {
    let mut iteration = 0u32;
    loop {
        loop {
            match deliver_one(&pool, sender.as_ref()).await {
                Ok(true) => continue,
                Ok(false) => break,
                Err(err) => {
                    tracing::warn!(%err, "notification delivery failed");
                    break;
                }
            }
        }
        iteration = iteration.wrapping_add(1);
        if iteration.is_multiple_of(HOUSEKEEPING_EVERY)
            && let Err(err) = outbox::housekeeping(&pool).await
        {
            tracing::warn!(%err, "upload housekeeping failed");
        }
        tokio::time::sleep(POLL).await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use sqlx::PgPool;

    use super::*;
    use crate::ingest::notice::Notice;

    #[derive(Default)]
    struct FakeSender {
        edits: Mutex<Vec<(String, String)>>,
        posts: Mutex<Vec<(u64, u64, String)>>,
        edit_result: Mutex<Vec<Result<(), SendError>>>,
        post_result: Mutex<Vec<Result<u64, SendError>>>,
    }

    #[async_trait]
    impl DiscordSender for FakeSender {
        async fn edit_original(&self, token: &str, content: &str) -> Result<(), SendError> {
            self.edits
                .lock()
                .unwrap()
                .push((token.to_owned(), content.to_owned()));
            self.edit_result.lock().unwrap().pop().unwrap_or(Ok(()))
        }
        async fn post_channel(
            &self,
            channel: u64,
            _user: u64,
            content: &str,
            nonce: &str,
        ) -> Result<u64, SendError> {
            self.posts
                .lock()
                .unwrap()
                .push((channel, 0, format!("{nonce}|{content}")));
            self.post_result.lock().unwrap().pop().unwrap_or(Ok(999))
        }
    }

    async fn request(pool: &PgPool, key: &str, token_valid: bool) {
        let minutes: i32 = if token_valid { 10 } else { -1 };
        sqlx::query(
            "INSERT INTO library_upload_requests (idempotency_key, application_id, guild_id,
                 channel_id, user_id, title, attachment_url, declared_size, interaction_token,
                 token_expires_at)
             VALUES ($1, 1, 2, 3, 4, 'song', 'https://cdn.discordapp.com/x', 10, 'tok',
                 now() + make_interval(mins => $2))",
        )
        .bind(key)
        .bind(minutes)
        .execute(pool)
        .await
        .unwrap();
        let mut conn = pool.acquire().await.unwrap();
        outbox::insert_notice(&mut conn, key, &Notice::Cancelled)
            .await
            .unwrap();
    }

    async fn state(pool: &PgPool, key: &str) -> (String, Option<String>, i32) {
        let row: (String, Option<String>, i32) = sqlx::query_as(
            "SELECT state, delivered_via, attempts FROM discord_notifications WHERE request_key = $1",
        )
        .bind(key)
        .fetch_one(pool)
        .await
        .unwrap();
        row
    }

    const KEY: &str = "discord-interaction:1";

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs DATABASE_URL (make test-db)"]
    async fn pg_notify_edits_the_original_reply_once(pool: PgPool) {
        request(&pool, KEY, true).await;
        let sender = FakeSender::default();
        assert!(deliver_one(&pool, &sender).await.unwrap());
        assert!(!deliver_one(&pool, &sender).await.unwrap()); // nothing left
        assert_eq!(sender.edits.lock().unwrap().len(), 1);
        assert!(sender.posts.lock().unwrap().is_empty());
        assert_eq!(
            state(&pool, KEY).await,
            ("delivered".into(), Some("interaction".into()), 1)
        );
        let token: Option<String> = sqlx::query_scalar(
            "SELECT interaction_token FROM library_upload_requests WHERE idempotency_key = $1",
        )
        .bind(KEY)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(token.is_none(), "token cleared after use");
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs DATABASE_URL (make test-db)"]
    async fn pg_notify_falls_back_to_the_channel_when_the_token_expired(pool: PgPool) {
        request(&pool, KEY, false).await;
        let sender = FakeSender::default();
        deliver_one(&pool, &sender).await.unwrap();
        assert!(sender.edits.lock().unwrap().is_empty());
        let posts = sender.posts.lock().unwrap().clone();
        assert_eq!(posts.len(), 1);
        assert!(
            posts[0]
                .2
                .starts_with(&crate::ingest::notice::nonce_for(KEY))
        );
        assert_eq!(state(&pool, KEY).await.1.as_deref(), Some("channel"));
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs DATABASE_URL (make test-db)"]
    async fn pg_notify_falls_back_when_the_original_reply_is_gone(pool: PgPool) {
        request(&pool, KEY, true).await;
        let sender = FakeSender::default();
        sender
            .edit_result
            .lock()
            .unwrap()
            .push(Err(SendError::TokenUnusable(
                "10008 unknown message".into(),
            )));
        deliver_one(&pool, &sender).await.unwrap();
        assert_eq!(sender.posts.lock().unwrap().len(), 1);
        assert_eq!(state(&pool, KEY).await.1.as_deref(), Some("channel"));
    }

    /// The bot dies after claiming (or after Discord accepted, before recording it): the lease
    /// expires and the same notification is sent again with the same nonce.
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs DATABASE_URL (make test-db)"]
    async fn pg_notify_recovers_a_crashed_delivery_with_the_same_nonce(pool: PgPool) {
        request(&pool, KEY, false).await;
        let first = outbox::claim(&pool).await.unwrap().unwrap(); // ... and the process dies
        assert!(outbox::claim(&pool).await.unwrap().is_none(), "leased");
        sqlx::query(
            "UPDATE discord_notifications SET lease_expires_at = now() - interval '1 second'",
        )
        .execute(&pool)
        .await
        .unwrap();
        let sender = FakeSender::default();
        assert!(deliver_one(&pool, &sender).await.unwrap());
        let posts = sender.posts.lock().unwrap().clone();
        assert!(posts[0].2.starts_with(&first.nonce));
        assert_eq!(
            state(&pool, KEY).await,
            ("delivered".into(), Some("channel".into()), 2)
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs DATABASE_URL (make test-db)"]
    async fn pg_notify_retries_transient_and_gives_up_on_permanent_errors(pool: PgPool) {
        request(&pool, KEY, false).await;
        let sender = FakeSender::default();
        sender
            .post_result
            .lock()
            .unwrap()
            .push(Err(SendError::Transient("429".into())));
        deliver_one(&pool, &sender).await.unwrap();
        assert_eq!(state(&pool, KEY).await.0, "pending");
        // Not due yet (backoff).
        assert!(!deliver_one(&pool, &sender).await.unwrap());
        sqlx::query("UPDATE discord_notifications SET next_attempt_at = now()")
            .execute(&pool)
            .await
            .unwrap();
        sender
            .post_result
            .lock()
            .unwrap()
            .push(Err(SendError::Permanent(
                "50013 missing permissions".into(),
            )));
        deliver_one(&pool, &sender).await.unwrap();
        assert_eq!(state(&pool, KEY).await.0, "undeliverable");
    }
}
