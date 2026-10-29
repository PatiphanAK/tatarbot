"""`DocumentSource` adapter: Markdown and text files under one directory, recursively.

The document id is the path relative to the directory; the title is the first Markdown
heading, else the file name. Hidden files and directories, other extensions, empty files and
files over `MAX_FILE_BYTES` are skipped (logged).
"""

import asyncio
import logging
from pathlib import Path

from agent_runtime.application.ports import DocumentSource
from agent_runtime.domain import Document
from agent_runtime.domain.knowledge import document_title

log = logging.getLogger(__name__)

EXTENSIONS = (".md", ".markdown", ".txt")
MAX_FILE_BYTES = 1_000_000


class DirectoryDocumentSource(DocumentSource):
    def __init__(self, root: str | Path):
        self._root = Path(root)

    async def load(self) -> list[Document]:
        return await asyncio.to_thread(self._load)

    def _load(self) -> list[Document]:
        if not self._root.is_dir():
            log.warning("knowledge directory %s does not exist", self._root)
            return []
        documents = []
        for path in sorted(self._root.rglob("*")):
            relative = path.relative_to(self._root)
            if any(part.startswith(".") for part in relative.parts):
                continue
            if not path.is_file() or path.suffix.lower() not in EXTENSIONS:
                continue
            if path.stat().st_size > MAX_FILE_BYTES:
                log.warning(
                    "skipping knowledge file %s: larger than %d bytes", relative, MAX_FILE_BYTES
                )
                continue
            try:
                text = path.read_text("utf-8")
            except (OSError, UnicodeDecodeError) as e:
                log.warning("skipping knowledge file %s: %s", relative, e)
                continue
            if text.strip():
                title = document_title(text, path.stem.replace("-", " ").replace("_", " "))
                documents.append(Document(relative.as_posix(), title, text))
        return documents
