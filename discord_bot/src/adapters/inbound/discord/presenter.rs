//! Domain -> Discord rendering: message limits and user-facing error text.

use crate::application::ports::ChatError;

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
