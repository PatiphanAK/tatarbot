"""Uploaded documents: what is accepted, how it is parsed, and who may retrieve it.

Nothing the client says about a file is trusted. Its name only suggests a title and must
have an accepted extension; its type is decided from the bytes; its size is enforced by
the presigned upload and checked again by the worker.
"""

import enum
import re
import unicodedata
from dataclasses import dataclass
from datetime import datetime
from pathlib import PurePosixPath

from agent_runtime.domain.errors import InvalidInputError
from agent_runtime.domain.knowledge import document_title

# Bump when parsing changes in a way that changes the extracted text: documents are then
# re-ingested on their next reprocess (it is part of the ingestion job's identity).
PARSER_VERSION = "text-v1"

# Namespaces scope retrieval: "global" for everyone, "discord:<guild id>" per server.
GLOBAL_NAMESPACE = "global"
NAMESPACE_PATTERN = r"^[a-z0-9][a-z0-9:_-]{0,199}$"
_NAMESPACE = re.compile(NAMESPACE_PATTERN)
_DISCORD_GUILD = re.compile(r"^discord:(\d{1,20}):")
_SOURCE_UNSAFE = re.compile(r"[\x00-\x1f\x7f]")
# Control characters other than whitespace: a sign of binary content.
_BINARY_CHARS = re.compile(r"[\x00-\x08\x0b\x0e-\x1f\x7f-\x9f]")

_EXTENSIONS = {".md": "text/markdown", ".markdown": "text/markdown", ".txt": "text/plain"}
# Signatures of common binary formats, for a clearer error than "not UTF-8".
_BINARY_SIGNATURES = {
    b"%PDF-": "PDF",
    b"PK\x03\x04": "ZIP/Office",
    b"\x89PNG": "PNG",
    b"\xff\xd8\xff": "JPEG",
    b"GIF8": "GIF",
    b"\xd0\xcf\x11\xe0": "legacy Office",
}


class DocumentStatus(enum.StrEnum):
    PENDING_UPLOAD = "pending_upload"
    QUEUED = "queued"
    PROCESSING = "processing"
    READY = "ready"
    FAILED = "failed"


class Claim(enum.StrEnum):
    """The outcome of trying to start an ingestion attempt."""

    CLAIMED = "claimed"
    # Another worker holds the job and its lease has not expired.
    BUSY = "busy"
    # Succeeded, skipped or failed (possibly just now, out of attempts).
    FINISHED = "finished"


class InvalidDocumentError(InvalidInputError):
    """The uploaded content can never be ingested (wrong type, too large, empty...).
    Retrying does not help; the message is shown to the uploader."""


@dataclass(frozen=True, slots=True)
class ParsedDocument:
    text: str
    mime_type: str


@dataclass(frozen=True, slots=True)
class PresignedUpload:
    url: str
    # Form fields the client posts with the file (the file field last).
    fields: dict[str, str]
    expires_at: datetime


@dataclass(frozen=True, slots=True)
class StoredObject:
    data: bytes
    # None when the bucket is not versioned.
    version: str | None


@dataclass(frozen=True, slots=True)
class JobKey:
    """What makes two ingestions the same: the object and the pipeline configuration."""

    document_id: int
    object_key: str
    object_version: str
    parser_version: str
    chunker_version: str
    embedding_model: str


@dataclass(frozen=True, slots=True)
class IngestionJob:
    id: int
    key: JobKey
    status: str
    # Attempts started so far, including the current one once claimed.
    attempts: int


@dataclass(frozen=True, slots=True)
class StoredDocument:
    """A row of rag.documents, as the upload API reports it."""

    id: int
    namespace: str
    source: str
    title: str | None
    status: DocumentStatus
    storage_provider: str | None
    storage_bucket: str | None
    object_key: str | None
    object_version: str | None
    mime_type: str | None
    size_bytes: int | None
    content_hash: str | None
    error: str | None
    chunks: int
    created_at: datetime
    updated_at: datetime
    processed_at: datetime | None


def validate_namespace(namespace: str) -> str:
    if not _NAMESPACE.match(namespace):
        raise InvalidInputError(
            "namespace must be 1-200 characters: lowercase letters, digits, ':', '_', '-'"
        )
    return namespace


def retrieval_namespaces(conversation_id: str) -> tuple[str, ...]:
    """What a chat turn may read: the global namespace, plus its Discord server's.

    The bot's conversation ids are `discord:<guild id | dm>:<channel>:<user>`; DMs and other
    callers only get the global namespace. Derived on the server, never from the request
    body, so one server's documents never reach another server's chats.
    """
    if m := _DISCORD_GUILD.match(conversation_id):
        return (GLOBAL_NAMESPACE, f"discord:{m.group(1)}")
    return (GLOBAL_NAMESPACE,)


def upload_source(filename: str, source: str | None) -> tuple[str, str]:
    """(source, extension) for an upload request. `source` identifies the document within
    its namespace (uploading the same source again replaces it); it defaults to the file
    name. Only the base name of `filename` is used, and only for its extension and as a
    fallback title."""
    name = unicodedata.normalize("NFC", PurePosixPath(filename.replace("\\", "/")).name).strip()
    extension = PurePosixPath(name).suffix.lower()
    if extension not in _EXTENSIONS:
        raise InvalidDocumentError(
            f"unsupported file type {extension or '(none)'}; accepted: "
            + ", ".join(sorted(_EXTENSIONS))
        )
    source = (source if source is not None else name).strip()
    if not source or len(source) > 2000 or _SOURCE_UNSAFE.search(source):
        raise InvalidInputError("source must be 1-2000 printable characters")
    return source, extension


def check_size(size_bytes: int, max_bytes: int) -> None:
    if size_bytes <= 0:
        raise InvalidDocumentError("the file is empty")
    if size_bytes > max_bytes:
        raise InvalidDocumentError(f"the file is larger than {max_bytes} bytes")


def parse_document(data: bytes, extension: str, max_bytes: int) -> ParsedDocument:
    """Text out of uploaded bytes: UTF-8 Markdown or plain text, nothing else.

    The type comes from the content (binary signatures, UTF-8 validity, control
    characters); the upload's extension only chooses between Markdown and plain text.
    """
    check_size(len(data), max_bytes)
    for signature, kind in _BINARY_SIGNATURES.items():
        if data.startswith(signature):
            raise InvalidDocumentError(f"{kind} files are not supported; upload Markdown or text")
    try:
        text = data.decode("utf-8-sig")
    except UnicodeDecodeError:
        raise InvalidDocumentError("the file is not UTF-8 text") from None
    if _BINARY_CHARS.search(text):
        raise InvalidDocumentError("the file contains binary data")
    text = unicodedata.normalize("NFC", text.replace("\r\n", "\n").replace("\r", "\n"))
    if not text.strip():
        raise InvalidDocumentError("the file has no text")
    return ParsedDocument(text, _EXTENSIONS.get(extension.lower(), "text/plain"))


def title_of(parsed: ParsedDocument, source: str) -> str:
    fallback = PurePosixPath(source).stem or source
    title = document_title(parsed.text, fallback) if parsed.mime_type == "text/markdown" else ""
    return (title or fallback)[:500]
