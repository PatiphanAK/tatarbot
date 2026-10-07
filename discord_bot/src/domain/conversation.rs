use std::fmt;

use super::DomainError;

/// Opaque identifier for a conversation. Adapters decide how to derive it
/// (e.g. from a Discord channel + user); the core only compares it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ConversationId(String);

impl ConversationId {
    pub fn new(value: impl Into<String>) -> Result<Self, DomainError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(DomainError::EmptyConversationId);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ConversationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_blank_ids() {
        assert_eq!(
            ConversationId::new("  "),
            Err(DomainError::EmptyConversationId)
        );
    }

    #[test]
    fn keeps_value() {
        assert_eq!(ConversationId::new("abc").unwrap().as_str(), "abc");
    }
}
