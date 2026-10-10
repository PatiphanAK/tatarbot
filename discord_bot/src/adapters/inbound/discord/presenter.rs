//! Domain -> Discord rendering: message limits and user-facing error text.

use std::time::Duration;

use crate::application::ports::{
    ChatError, LibraryError, LibraryPage, MusicError, PlayOutcome, ResolveError, UploadOutcome,
    VoiceError,
};
use crate::domain::library::LibraryTrack;
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
            ResolveError::LibraryUnavailable => LIBRARY_UNAVAILABLE.into(),
            ResolveError::NotFound(query) => format!(
                "🔎 Nothing in this server's library matches `{}`. See `/library`, or paste a link.",
                escape(query)
            ),
        },
        MusicError::Voice(VoiceError::Join(_)) => {
            "🔌 I couldn't join that voice channel. Check that I can Connect and Speak there."
                .into()
        }
        MusicError::Voice(_) => GENERIC_FAILURE.into(),
    }
}

// ---- Library

const LIBRARY_UNAVAILABLE: &str =
    "🗄️ The music library is temporarily unavailable. Radio and links still work.";

pub fn uploaded(outcome: &UploadOutcome) -> String {
    format!(
        "📥 Added {} to the library. Play it with `/play {}`.",
        library_entry(&outcome.track),
        escape(&outcome.track.title)
    )
}

pub fn deleted(track: &LibraryTrack) -> String {
    format!("🗑️ Removed **{}** from the library.", escape(&track.title))
}

pub fn library_page(page: &LibraryPage) -> String {
    let usage = format!(
        "{} of {} used",
        megabytes(page.used_bytes),
        megabytes(page.quota_bytes)
    );
    if page.tracks.is_empty() {
        return format!("📚 The library is empty ({usage}). Add audio with `/upload`.");
    }
    let mut out = format!(
        "📚 **Library**: {} tracks, {usage}. Page {}/{}",
        page.total, page.page, page.pages
    );
    for track in &page.tracks {
        out.push_str(&format!("\n• {}", library_entry(track)));
    }
    out
}

pub fn library_error(err: &LibraryError) -> String {
    match err {
        LibraryError::Unavailable => LIBRARY_UNAVAILABLE.into(),
        LibraryError::NotAllowed => {
            "🔒 You need the upload role (or Manage Server) for that.".into()
        }
        LibraryError::TooLarge { max_bytes } => {
            format!("📦 That file is too big (max {}).", megabytes(*max_bytes))
        }
        LibraryError::TooLong { max } => {
            format!("⏱️ That's too long (max {}).", duration(*max))
        }
        LibraryError::NotAudio => "🎧 That isn't an audio file I can use \
             (mp3, m4a/aac, ogg/opus/vorbis, flac, wav, mkv/webm)."
            .into(),
        LibraryError::AlreadyInLibrary(track) => {
            format!(
                "📚 That's already in the library as **{}**.",
                escape(&track.title)
            )
        }
        LibraryError::QuotaExceeded {
            used_bytes,
            quota_bytes,
        } => format!(
            "📦 Not enough library space: {} of {} used. Delete something with `/delete`.",
            megabytes(*used_bytes),
            megabytes(*quota_bytes)
        ),
        LibraryError::NotFound => "🔎 That track isn't in this server's library.".into(),
        LibraryError::InvalidTitle => "🏷️ Titles must be 1-100 characters.".into(),
        LibraryError::Processing(_) => {
            "⚙️ I couldn't process that file. Try another format or a shorter file.".into()
        }
        LibraryError::Internal(_) => GENERIC_FAILURE.into(),
    }
}

/// Autocomplete label: "Title (3:05)", within Discord's 100-character limit.
pub fn library_choice(track: &LibraryTrack) -> String {
    let suffix = format!(" ({})", duration(track.duration));
    let room = 100 - suffix.chars().count();
    let title: String = track.title.chars().take(room).collect();
    format!("{title}{suffix}")
}

fn library_entry(track: &LibraryTrack) -> String {
    format!(
        "**{}** ({}) · added by <@{}>",
        escape(&track.title),
        duration(track.duration),
        track.uploaded_by.0
    )
}

fn duration(d: Duration) -> String {
    let secs = d.as_secs();
    format!("{}:{:02}", secs / 60, secs % 60)
}

fn megabytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
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

    fn library_track(title: &str, secs: u64) -> LibraryTrack {
        use crate::domain::library::{ContentHash, LibraryTrackId};
        use crate::domain::music::{GuildId, UserId};
        LibraryTrack {
            id: LibraryTrackId(1),
            guild: GuildId(1),
            title: title.into(),
            hash: ContentHash::from_digest(&[1; 32]),
            duration: Duration::from_secs(secs),
            size_bytes: 2 * 1024 * 1024,
            uploaded_by: UserId(42),
            created_at: chrono::DateTime::<chrono::Utc>::UNIX_EPOCH,
        }
    }

    #[test]
    fn renders_library_results() {
        let track = library_track("My *Song*", 185);
        assert_eq!(
            uploaded(&UploadOutcome {
                track: track.clone(),
                reused_audio: false
            }),
            r"📥 Added **My \*Song\*** (3:05) · added by <@42> to the library. Play it with `/play My \*Song\*`."
        );
        let page = LibraryPage {
            tracks: vec![track],
            page: 1,
            pages: 3,
            total: 21,
            used_bytes: 5 * 1024 * 1024,
            quota_bytes: 500 * 1024 * 1024,
        };
        let text = library_page(&page);
        assert!(text.starts_with("📚 **Library**: 21 tracks, 5.0 MB of 500.0 MB used. Page 1/3"));
        assert!(text.contains("\n• **My \\*Song\\*** (3:05)"));
    }

    #[test]
    fn autocomplete_labels_fit_discord_limits() {
        let label = library_choice(&library_track(&"x".repeat(100), 59));
        assert_eq!(label.chars().count(), 100);
        assert!(label.ends_with(" (0:59)"));
    }

    #[test]
    fn library_errors_never_leak_details() {
        for err in [
            LibraryError::Processing("ffmpeg: /tmp/secret path".into()),
            LibraryError::Internal("db password=hunter2".into()),
        ] {
            let text = library_error(&err);
            assert!(
                !text.contains("secret") && !text.contains("hunter2"),
                "{text}"
            );
        }
        assert!(library_error(&LibraryError::Unavailable).contains("Radio and links still work"));
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
