//! Discord -> domain conversions.

use serenity::all::{ChannelId, GuildId, UserId};

use crate::domain::ConversationId;

/// One conversation per user per channel (threads are channels, so each
/// thread gets its own history). DMs use the `dm` scope.
pub fn conversation_id(guild: Option<GuildId>, channel: ChannelId, user: UserId) -> ConversationId {
    let scope = guild.map_or_else(|| "dm".to_owned(), |g| g.get().to_string());
    ConversationId::new(format!("discord:{scope}:{}:{}", channel.get(), user.get()))
        .expect("formatted id is never empty")
}

/// Removes `<@id>` / `<@!id>` mentions of the bot from a message.
pub fn strip_bot_mention(content: &str, bot: UserId) -> String {
    content
        .replace(&format!("<@{}>", bot.get()), "")
        .replace(&format!("<@!{}>", bot.get()), "")
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_guild_and_dm_ids() {
        let id = conversation_id(Some(GuildId::new(1)), ChannelId::new(2), UserId::new(3));
        assert_eq!(id.as_str(), "discord:1:2:3");
        let id = conversation_id(None, ChannelId::new(2), UserId::new(3));
        assert_eq!(id.as_str(), "discord:dm:2:3");
    }

    #[test]
    fn strips_mentions() {
        let bot = UserId::new(42);
        assert_eq!(strip_bot_mention("<@42> hello <@!42>", bot), "hello");
        assert_eq!(strip_bot_mention("hi <@7>", bot), "hi <@7>");
    }
}
