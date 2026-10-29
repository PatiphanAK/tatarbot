"""`EmbeddingPort` decorator that keeps document vectors in a JSON file.

A reload or restart then embeds only new or changed chunks. Entries are keyed by a hash of
the text; the file also records the wrapped embedder's `identity`, and a file written by
another model, dimension or prefix setting is ignored, so switching the embedding model
re-embeds everything once. Each `embed_documents` call rewrites the file with exactly the
vectors of that call, which drops chunks that no longer exist.

Queries are never cached. The file is written atomically (temp file + rename).
"""

import asyncio
import hashlib
import json
import logging
import os
from collections.abc import Sequence
from pathlib import Path

from agent_runtime.application.ports import EmbeddingPort

log = logging.getLogger(__name__)

FORMAT_VERSION = 1


class CachedEmbeddings(EmbeddingPort):
    def __init__(self, inner: EmbeddingPort, path: str | Path):
        self._inner = inner
        self._path = Path(path)

    @property
    def identity(self) -> str:
        return self._inner.identity

    async def embed_documents(self, texts: Sequence[str]) -> list[list[float]]:
        cached = await asyncio.to_thread(self._read)
        keys = [_key(t) for t in texts]
        missing = list(dict.fromkeys(k for k in keys if k not in cached))
        if missing:
            by_key = dict(zip(keys, texts, strict=True))
            fresh = await self._inner.embed_documents([by_key[k] for k in missing])
            cached.update(zip(missing, fresh, strict=True))
        log.info("embedding cache hits=%d embedded=%d", len(keys) - len(missing), len(missing))
        vectors = [cached[k] for k in keys]
        await asyncio.to_thread(self._write, dict(zip(keys, vectors, strict=True)))
        return vectors

    async def embed_query(self, text: str) -> list[float]:
        return await self._inner.embed_query(text)

    async def aclose(self) -> None:
        await self._inner.aclose()

    def _read(self) -> dict[str, list[float]]:
        try:
            data = json.loads(self._path.read_text("utf-8"))
        except FileNotFoundError:
            return {}
        except (OSError, ValueError) as e:
            log.warning("ignoring unreadable embedding cache %s: %s", self._path, e)
            return {}
        if data.get("version") != FORMAT_VERSION or data.get("identity") != self.identity:
            log.info("embedding cache is for another embedder; re-embedding everything")
            return {}
        return data.get("vectors", {})

    def _write(self, vectors: dict[str, list[float]]) -> None:
        payload = {"version": FORMAT_VERSION, "identity": self.identity, "vectors": vectors}
        tmp = self._path.with_name(self._path.name + ".tmp")
        try:
            self._path.parent.mkdir(parents=True, exist_ok=True)
            tmp.write_text(json.dumps(payload, separators=(",", ":")), "utf-8")
            os.replace(tmp, self._path)
        except OSError as e:
            # A read-only filesystem only costs re-embedding on the next start.
            log.warning("cannot write embedding cache %s: %s", self._path, e)


def _key(text: str) -> str:
    return hashlib.sha256(text.encode()).hexdigest()
