//! Domain -> Discord rendering: message limits and user-facing error text.

use crate::application::ports::{ChatError, MusicError, PlayOutcome, ResolveError, VoiceError};
use crate::domain::music::TrackInfo;

/// Discord's maximum message length, in characters.
pub const DISCORD_MESSAGE_LIMIT: usize = 2000;

const FENCE: &str = "```";
const REOPEN_FENCE: &str = "```\n";
const CLOSE_FENCE: &str = "\n```";

const GENERIC_FAILURE: &str = "💥 Something went wrong on my side. Please try again.";
pub const EMPTY_PROMPT: &str =
    "👋 Mention me with a message, e.g. `@TatarBot hello`, or reply to one of my messages.";

pub fn error_message(err: &ChatError) -> String {
    match err {
        ChatError::AgentUnavailable(_) => {
            "⚠️ I can't reach my brain right now. Please try again in a moment.".into()
        }
        ChatError::Timeout => "⌛ That took too long to answer. Please try again.".into(),
        ChatError::Rejected(reason) => format!("🚫 I couldn't process that request: {reason}"),
        ChatError::EmptyReply => "🤔 I didn't come up with a reply. Try rephrasing?".into(),
        ChatError::Internal(_) => GENERIC_FAILURE.into(),
    }
}

/// Splits text into Discord-sized messages, preferring newline then space
/// boundaries, and closing/reopening code fences cut across chunks.
pub fn split_message(text: &str) -> Vec<String> {
    let budget = DISCORD_MESSAGE_LIMIT - REOPEN_FENCE.len() - CLOSE_FENCE.len();
    let mut chunks = Vec::new();
    let mut rest = text.trim();
    let mut in_fence = false;

    while !rest.is_empty() {
        let (head, tail) = take_chunk(rest, budget);
        let mut chunk = if in_fence {
            format!("{REOPEN_FENCE}{head}")
        } else {
            head.to_owned()
        };
        in_fence = chunk.matches(FENCE).count() % 2 == 1;
        if in_fence && !tail.is_empty() {
            chunk.push_str(CLOSE_FENCE);
        }
        chunks.push(chunk);
        rest = tail;
    }
    chunks
}

// ---- Music

pub const MUSIC_GUILD_ONLY: &str = "🎵 Music commands work in servers only.";
pub const MUSIC_STOPPED: &str = "⏹️ Stopped and cleared the queue.";
pub const MUSIC_LEFT: &str = "👋 Left the voice channel.";
const QUEUE_SHOWN: usize = 10;

pub fn play_outcome(outcome: &PlayOutcome) -> String {
    match outcome {
        PlayOutcome::NowPlaying(track) => format!("▶️ Now playing {}", describe(track)),
        PlayOutcome::Queued { track, position } => {
            format!("➕ Queued #{position}: {}", describe(track))
        }
    }
}

pub fn skipped(next: Option<&TrackInfo>) -> String {
    match next {
        Some(track) => format!("⏭️ Skipped. Now playing {}", describe(track)),
        None => "⏭️ Skipped. The queue is empty.".into(),
    }
}

pub fn queue(tracks: &[TrackInfo]) -> String {
    let Some((current, rest)) = tracks.split_first() else {
        return "📭 Nothing is playing. Try `/radio` or `/play <link>`.".into();
    };
    let mut out = format!("▶️ {}", describe(current));
    for (i, track) in rest.iter().take(QUEUE_SHOWN).enumerate() {
        out.push_str(&format!("\n`{}.` {}", i + 1, describe(track)));
    }
    if rest.len() > QUEUE_SHOWN {
        out.push_str(&format!("\n…and {} more", rest.len() - QUEUE_SHOWN));
    }
    out
}

pub fn music_error(err: &MusicError) -> String {
    match err {
        MusicError::NotInVoice => "🔇 Join a voice channel first.".into(),
        MusicError::NotInBotChannel(channel) => {
            format!("🔇 Join <#{}> to control playback.", channel.0)
        }
        MusicError::BusyElsewhere(channel) => {
            format!(
                "🎧 I'm playing in <#{}> right now. Join it, or wait until it's done.",
                channel.0
            )
        }
        MusicError::NothingPlaying => "📭 Nothing is playing.".into(),
        MusicError::UnknownStation(name) => {
            format!("📻 I don't know a station called `{}`.", escape(name))
        }
        MusicError::QueueFull(max) => format!("📚 The queue is full ({max} tracks)."),
        MusicError::Resolve(err) => match err {
            ResolveError::SearchUnavailable => "🔎 Searching by title isn't available yet. \
                 Paste a direct audio link, or try `/radio`."
                .into(),
            ResolveError::InvalidUrl(_) => "🔗 That doesn't look like a valid link.".into(),
            ResolveError::UnsupportedScheme => "🔗 Only http and https links can be played.".into(),
            ResolveError::ForbiddenAddress => {
                "🚫 Links to private or local addresses aren't allowed.".into()
            }
        },
        MusicError::Voice(VoiceError::Join(_)) => {
            "🔌 I couldn't join that voice channel. Check that I can Connect and Speak there."
                .into()
        }
        MusicError::Voice(_) => GENERIC_FAILURE.into(),
    }
}

fn describe(track: &TrackInfo) -> String {
    let live = if track.live { " 🔴 LIVE" } else { "" };
    format!(
        "**{}**{live} · requested by <@{}>",
        escape(&track.title),
        track.requested_by.0
    )
}

/// Titles come from user links: neutralise Discord markdown and mentions.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(
            c,
            '*' | '_' | '~' | '`' | '|' | '>' | '#' | '[' | ']' | '(' | ')' | '\\' | '<' | '@'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Returns `(head, tail)` where `head` has at most `max_chars` characters.
fn take_chunk(s: &str, max_chars: usize) -> (&str, &str) {
    let Some((end, _)) = s.char_indices().nth(max_chars) else {
        return (s, "");
    };
    let window = &s[..end];
    let min = end / 2;
    match window.rfind('\n').filter(|&i| i > min) {
        Some(i) => (&s[..i], &s[i + 1..]),
        None => match window.rfind(' ').filter(|&i| i > min) {
            Some(i) => (&s[..i], &s[i + 1..]),
            None => (window, &s[end..]),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_within_limit(chunks: &[String]) {
        for c in chunks {
            assert!(
                c.chars().count() <= DISCORD_MESSAGE_LIMIT,
                "chunk too long: {}",
                c.len()
            );
        }
    }

    #[test]
    fn short_text_is_one_chunk() {
        assert_eq!(split_message("  hello  "), vec!["hello"]);
        assert!(split_message("   ").is_empty());
    }

    #[test]
    fn splits_on_newlines() {
        let line = "a".repeat(1500);
        let text = format!("{line}\n{line}");
        let chunks = split_message(&text);
        assert_eq!(chunks, vec![line.clone(), line]);
    }

    #[test]
    fn hard_splits_without_whitespace_and_respects_multibyte() {
        let text = "é".repeat(5000);
        let chunks = split_message(&text);
        assert_within_limit(&chunks);
        assert_eq!(chunks.concat(), text);
    }

    fn track(title: &str, live: bool) -> TrackInfo {
        TrackInfo {
            title: title.into(),
            live,
            requested_by: crate::domain::music::UserId(42),
        }
    }

    #[test]
    fn renders_music_results() {
        assert_eq!(
            play_outcome(&PlayOutcome::NowPlaying(track("Lofi", true))),
            "▶️ Now playing **Lofi** 🔴 LIVE · requested by <@42>"
        );
        assert_eq!(
            play_outcome(&PlayOutcome::Queued {
                track: track("a.mp3", false),
                position: 2
            }),
            "➕ Queued #2: **a.mp3** · requested by <@42>"
        );
        assert_eq!(skipped(None), "⏭️ Skipped. The queue is empty.");
    }

    #[test]
    fn queue_lists_current_then_up_to_ten_more() {
        assert!(queue(&[]).starts_with("📭"));
        let tracks: Vec<_> = (0..13).map(|i| track(&format!("t{i}"), false)).collect();
        let text = queue(&tracks);
        assert!(text.starts_with("▶️ **t0**"));
        assert!(text.contains("`10.` **t10**") && !text.contains("**t11**"));
        assert!(text.ends_with("…and 2 more"));
    }

    #[test]
    fn titles_cannot_inject_markdown_or_mentions() {
        let text = play_outcome(&PlayOutcome::NowPlaying(track(
            "**x** @everyone <@1>",
            false,
        )));
        assert!(text.contains(r"\*\*x\*\* \@everyone \<\@1\>"), "{text}");
    }

    #[test]
    fn every_music_error_has_text() {
        use crate::domain::music::VoiceChannelId;
        let errors = [
            MusicError::NotInVoice,
            MusicError::NotInBotChannel(VoiceChannelId(5)),
            MusicError::BusyElsewhere(VoiceChannelId(5)),
            MusicError::NothingPlaying,
            MusicError::UnknownStation("x".into()),
            MusicError::QueueFull(50),
            MusicError::Resolve(ResolveError::SearchUnavailable),
            MusicError::Voice(VoiceError::Join("boom".into())),
            MusicError::Voice(VoiceError::Other("secret detail".into())),
        ];
        for err in errors {
            let text = music_error(&err);
            assert!(!text.is_empty() && !text.contains("secret detail") && !text.contains("boom"));
        }
        assert!(music_error(&MusicError::NotInBotChannel(VoiceChannelId(5))).contains("<#5>"));
    }

    #[test]
    fn reopens_code_fences() {
        let code: String = (0..400).map(|i| format!("let x{i} = {i};\n")).collect();
        let text = format!("Here:\n```rust\n{code}```\nDone.");
        let chunks = split_message(&text);
        assert!(chunks.len() > 1);
        assert_within_limit(&chunks);
        for c in &chunks {
            assert_eq!(c.matches(FENCE).count() % 2, 0, "unbalanced fence in {c:?}");
        }
    }
}
