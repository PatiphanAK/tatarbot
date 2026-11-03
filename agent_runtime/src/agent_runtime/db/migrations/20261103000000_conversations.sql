-- Conversation history for CONVERSATION_STORE=postgres, so history survives runtime
-- restarts, deploys and Cloud Run scaling to zero. One row per conversation; `messages` is
-- the trimmed history (system prompt excluded), oldest first, as the runtime's
-- ChatMessage list. Rows idle longer than CONVERSATION_RETENTION_DAYS are deleted by the
-- runtime.

CREATE SCHEMA IF NOT EXISTS chat;

CREATE TABLE chat.conversations (
    conversation_id TEXT        PRIMARY KEY CHECK (char_length(conversation_id) BETWEEN 1 AND 256),
    messages        JSONB       NOT NULL CHECK (jsonb_typeof(messages) = 'array'),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Retention sweep.
CREATE INDEX conversations_updated_at_idx ON chat.conversations (updated_at);
