"""`ConversationStore` in Postgres (`runtime.conversations`, migration 20261103000000).

History survives runtime restarts, deploys and Cloud Run scaling to zero. Each
conversation is one row holding its trimmed message list as JSONB; every change runs in
one transaction that locks the row, so concurrent turns can't lose each other's messages
(the chat service also serializes turns per conversation within a process).

A database problem never fails a chat turn: `load` returns no history, and `append` /
`replace_last` drop the change, each with a warning, the same way the runtime keeps
answering without knowledge when the RAG store is down. Statements are never prepared, so
a transaction pooler works too.
"""

import logging
import time
from collections.abc import Sequence

from psycopg import Error as PsycopgError
from psycopg.types.json import Jsonb
from psycopg_pool import AsyncConnectionPool

from agent_runtime.application.ports import ConversationStore
from agent_runtime.domain import ChatMessage, Role, ToolCall

log = logging.getLogger(__name__)

# Per statement; a stuck database must not eat the turn's budget.
_STATEMENT_TIMEOUT = "5s"
# The retention sweep runs at most this often per process, after a write.
_SWEEP_INTERVAL_SECONDS = 3600


def to_json(message: ChatMessage) -> dict:
    data: dict = {"role": message.role.value, "content": message.content}
    if message.tool_calls:
        data["tool_calls"] = [
            {"id": c.id, "name": c.name, "arguments": c.arguments} for c in message.tool_calls
        ]
    if message.tool_call_id is not None:
        data["tool_call_id"] = message.tool_call_id
    return data


def from_json(data: dict) -> ChatMessage:
    return ChatMessage(
        Role(data["role"]),
        data["content"],
        tuple(ToolCall(c["id"], c["name"], c["arguments"]) for c in data.get("tool_calls", ())),
        data.get("tool_call_id"),
    )


class PostgresConversationStore(ConversationStore):
    def __init__(
        self,
        pool: AsyncConnectionPool,
        max_messages_per_conversation: int = 100,
        retention_days: int = 30,
    ):
        self._pool = pool
        self._max_messages = max_messages_per_conversation
        self._retention_days = retention_days
        self._last_sweep = 0.0

    async def load(self, conversation_id: str) -> list[ChatMessage]:
        try:
            async with self._pool.connection() as conn, conn.transaction():
                await conn.execute(f"SET LOCAL statement_timeout = '{_STATEMENT_TIMEOUT}'")
                row = await (
                    await conn.execute(
                        "SELECT messages FROM runtime.conversations WHERE conversation_id = %s",
                        (conversation_id,),
                    )
                ).fetchone()
        except PsycopgError as e:
            log.warning("history unavailable conversation_id=%s error=%s", conversation_id, e)
            return []
        return [from_json(m) for m in row[0]] if row else []

    async def append(self, conversation_id: str, messages: Sequence[ChatMessage]) -> None:
        if not messages:
            return

        def update(stored: list[ChatMessage]) -> list[ChatMessage]:
            combined = [*stored, *messages]
            return combined[max(0, len(combined) - self._max_messages) :]

        await self._modify(conversation_id, update, "append")
        await self._maybe_sweep()

    async def replace_last(self, conversation_id: str, messages: Sequence[ChatMessage]) -> bool:
        n = len(messages)

        def update(stored: list[ChatMessage]) -> list[ChatMessage] | None:
            if not stored or n == 0 or len(stored) < n:
                return None
            pairs = zip(stored[-n:], messages, strict=True)
            if any(old.role is not new.role for old, new in pairs):
                return None
            return [*stored[:-n], *messages]

        return await self._modify(conversation_id, update, "replace_last")

    async def _modify(self, conversation_id, update, operation: str) -> bool:
        """Lock the row, apply `update` to its messages, write the result. `update`
        returns None to leave the row alone. False if nothing was written."""
        try:
            async with self._pool.connection() as conn, conn.transaction():
                await conn.execute(f"SET LOCAL statement_timeout = '{_STATEMENT_TIMEOUT}'")
                row = await (
                    await conn.execute(
                        "SELECT messages FROM runtime.conversations"
                        " WHERE conversation_id = %s FOR UPDATE",
                        (conversation_id,),
                    )
                ).fetchone()
                stored = [from_json(m) for m in row[0]] if row else []
                updated = update(stored)
                if updated is None:
                    return False
                await conn.execute(
                    "INSERT INTO runtime.conversations (conversation_id, messages, updated_at)"
                    " VALUES (%s, %s, now())"
                    " ON CONFLICT (conversation_id) DO UPDATE"
                    " SET messages = EXCLUDED.messages, updated_at = now()",
                    (conversation_id, Jsonb([to_json(m) for m in updated])),
                )
                return True
        except PsycopgError as e:
            log.warning(
                "history not saved conversation_id=%s operation=%s error=%s",
                conversation_id,
                operation,
                e,
            )
            return False

    async def _maybe_sweep(self) -> None:
        now = time.monotonic()
        if self._last_sweep and now - self._last_sweep < _SWEEP_INTERVAL_SECONDS:
            return
        self._last_sweep = now
        try:
            async with self._pool.connection() as conn, conn.transaction():
                await conn.execute(f"SET LOCAL statement_timeout = '{_STATEMENT_TIMEOUT}'")
                cur = await conn.execute(
                    "DELETE FROM runtime.conversations"
                    " WHERE updated_at < now() - make_interval(days => %s)",
                    (self._retention_days,),
                )
            if cur.rowcount:
                log.info("expired conversations deleted=%d", cur.rowcount)
        except PsycopgError as e:
            log.warning("conversation retention sweep failed error=%s", e)
