"""Knowledge base types and chunking, independent of the embedding model and vector store.

A `Document` is split into `Chunk`s along its Markdown structure: sections (headings), then
paragraphs packed up to `max_chars`. Only a paragraph longer than `max_chars` is cut
mid-text, into windows that overlap by `overlap` characters. Each chunk remembers its
heading path, which is embedded with it so a chunk like "Run `make up`." still carries
"Development > Local stack".
"""

import hashlib
import re
from dataclasses import dataclass

_HEADING = re.compile(r"^(#{1,6})\s+(.+?)\s*#*\s*$")
_BLANK_LINES = re.compile(r"\n\s*\n")


@dataclass(frozen=True, slots=True)
class Document:
    # Stable id, e.g. the path relative to the knowledge directory.
    id: str
    title: str
    text: str


@dataclass(frozen=True, slots=True)
class Chunk:
    document_id: str
    title: str
    # Heading path inside the document, e.g. "Music > Uploads"; empty before any heading.
    heading: str
    text: str
    # Position within the document, from 0.
    index: int

    @property
    def embedding_text(self) -> str:
        """What gets embedded: the location in the document, then the text."""
        location = " > ".join(p for p in (self.title, self.heading) if p)
        return f"{location}\n{self.text}" if location else self.text

    @property
    def key(self) -> str:
        """Content hash: the same text in the same place embeds to the same vector."""
        return hashlib.sha256(self.embedding_text.encode()).hexdigest()


@dataclass(frozen=True, slots=True)
class RetrievedChunk:
    chunk: Chunk
    # Cosine similarity to the query, in [-1, 1]; higher is closer.
    score: float


def document_title(text: str, fallback: str) -> str:
    """The first Markdown heading, else `fallback` (e.g. the file name)."""
    for line in text.splitlines():
        if m := _HEADING.match(line.strip()):
            return m.group(2)
    return fallback


def chunk_document(document: Document, max_chars: int = 1200, overlap: int = 200) -> list[Chunk]:
    if max_chars < 100:
        raise ValueError("max_chars must be at least 100")
    if not 0 <= overlap < max_chars // 2:
        raise ValueError("overlap must be in [0, max_chars / 2)")

    chunks: list[Chunk] = []
    for heading, body in _sections(document.text):
        for text in _pack(_paragraphs(body), max_chars, overlap):
            chunks.append(Chunk(document.id, document.title, heading, text, len(chunks)))
    return chunks


def _sections(text: str) -> list[tuple[str, str]]:
    """(heading path, body) pairs. The document's title heading is not repeated in paths."""
    sections: list[tuple[str, list[str]]] = [("", [])]
    path: list[tuple[int, str]] = []
    in_code = False
    for line in text.splitlines():
        if line.lstrip().startswith("```"):
            in_code = not in_code
        m = None if in_code else _HEADING.match(line.strip())
        if m is None:
            sections[-1][1].append(line)
            continue
        level = len(m.group(1))
        path = [(lv, h) for lv, h in path if lv < level] + [(level, m.group(2))]
        # A lone top-level heading is the title, which every chunk already carries.
        shown = [h for lv, h in path if lv > 1] if path[0][0] == 1 else [h for _, h in path]
        sections.append((" > ".join(shown), []))
    return [(h, "\n".join(lines)) for h, lines in sections if "\n".join(lines).strip()]


def _paragraphs(body: str) -> list[str]:
    return [p.strip() for p in _BLANK_LINES.split(body) if p.strip()]


def _pack(paragraphs: list[str], max_chars: int, overlap: int) -> list[str]:
    out: list[str] = []
    current = ""
    for p in paragraphs:
        if len(p) > max_chars:
            if current:
                out.append(current)
                current = ""
            out.extend(_windows(p, max_chars, overlap))
        elif current and len(current) + 2 + len(p) > max_chars:
            out.append(current)
            current = p
        else:
            current = f"{current}\n\n{p}" if current else p
    if current:
        out.append(current)
    return out


def _windows(text: str, size: int, overlap: int) -> list[str]:
    """Fixed windows, each ending at whitespace when there is some in its last quarter
    (Thai has no spaces between words, so a hard cut is sometimes unavoidable)."""
    out = []
    start = 0
    while start < len(text):
        end = min(start + size, len(text))
        if end < len(text):
            cut = text.rfind(" ", start + size * 3 // 4, end)
            if cut == -1:
                cut = text.rfind("\n", start + size * 3 // 4, end)
            if cut != -1:
                end = cut
        out.append(text[start:end].strip())
        if end >= len(text):
            break
        start = max(end - overlap, start + 1)
    return [w for w in out if w]
