"""Inbound HTTP adapter: translates HTTP to `ChatService` calls.

Wire contract (unchanged from TatarBot's `AgentPort` HTTP adapter):

    POST /v1/chat  {"message": str, "conversation_id": str}
    200 {"message": str, "conversation_id": str}
    4xx/5xx {"error": str}

Knowledge base (operators, same key): GET /v1/knowledge, POST /v1/knowledge/reload.

Uploads (RAG_STORE=postgres with RAG_S3_BUCKET; same key), scoped to a namespace that the
caller names and is trusted to have authorized (the key is only held by TatarBot and
operators; per-user permissions are the caller's job):

    POST   /v1/knowledge/uploads                     presigned POST for a new document
    GET    /v1/knowledge/documents?namespace=        documents and their status
    GET    /v1/knowledge/documents/{id}?namespace=
    POST   /v1/knowledge/documents/{id}/reprocess?namespace=[&force=true]
    DELETE /v1/knowledge/documents/{id}?namespace=
"""

import logging
import secrets
from datetime import datetime
from typing import Annotated, Literal

from fastapi import APIRouter, Depends, FastAPI, Query, Request, Response
from fastapi.exceptions import RequestValidationError
from fastapi.responses import JSONResponse
from fastapi.security import HTTPAuthorizationCredentials, HTTPBearer
from pydantic import BaseModel, Field, field_validator

from agent_runtime.application import ChatService, IndexStatus, KnowledgeBase, ModelPort, Uploads
from agent_runtime.domain import (
    EmbeddingError,
    InvalidInputError,
    ModelError,
    ModelRequestRejectedError,
    ModelResponseError,
    ModelTimeoutError,
    ModelUnavailableError,
    StorageError,
    StoredDocument,
)
from agent_runtime.domain.client_tools import (
    ActionResult,
    ClientTools,
    MusicAction,
    MusicState,
    MusicTool,
    PlayingTrack,
    Station,
)
from agent_runtime.domain.ingestion import NAMESPACE_PATTERN

log = logging.getLogger(__name__)

MAX_MESSAGE_LENGTH = 16_000


class StationBody(BaseModel):
    name: str = Field(pattern=r"^[a-z0-9-]{1,32}$")
    title: str = Field(min_length=1, max_length=100)


class PlayingTrackBody(BaseModel):
    title: str = Field(min_length=1, max_length=200)
    live: bool = False
    song: str | None = Field(default=None, max_length=200)


class MusicStateBody(BaseModel):
    """What the bot is doing now (voice channel name, current track first)."""

    channel: str | None = Field(default=None, max_length=100)
    queue: list[PlayingTrackBody] = Field(default_factory=list, max_length=11)


class ActionResultBody(BaseModel):
    """The outcome of an action the bot carried out after an earlier turn."""

    action: Literal["play", "play_track", "skip", "stop", "queue"]
    station: str | None = Field(default=None, max_length=32)
    track: str | None = Field(default=None, max_length=100)
    ok: bool
    detail: str = Field(default="", max_length=300)


class MusicToolBody(BaseModel):
    stations: list[StationBody] = Field(min_length=1, max_length=25)
    state: MusicStateBody | None = None
    results: list[ActionResultBody] = Field(default_factory=list, max_length=4)
    # The server has an uploaded library; recent titles help match requests.
    library: bool = False
    library_hints: list[str] = Field(default_factory=list, max_length=20)

    @field_validator("library_hints")
    @classmethod
    def bounded_hints(cls, hints: list[str]) -> list[str]:
        if any(len(h) > 100 for h in hints):
            raise ValueError("library hints must be at most 100 characters")
        return hints

    @field_validator("stations")
    @classmethod
    def unique_names(cls, stations: list[StationBody]) -> list[StationBody]:
        if len({s.name for s in stations}) != len(stations):
            raise ValueError("station names must be unique")
        return stations


class ClientToolsBody(BaseModel):
    """Tools the caller can carry out itself; the model may request them."""

    music: MusicToolBody | None = None

    def to_domain(self) -> ClientTools:
        if self.music is None:
            return ClientTools()
        music = self.music
        stations = tuple(Station(s.name, s.title) for s in music.stations)
        state = music.state and MusicState(
            channel=music.state.channel,
            queue=tuple(PlayingTrack(t.title, t.live, t.song) for t in music.state.queue),
        )
        results = tuple(
            ActionResult(MusicAction(r.action), r.station, r.ok, r.detail, r.track)
            for r in music.results
        )
        return ClientTools(
            music=MusicTool(stations, state, results, music.library, tuple(music.library_hints))
        )


class ChatRequestBody(BaseModel):
    message: str = Field(min_length=1, max_length=MAX_MESSAGE_LENGTH)
    conversation_id: str = Field(min_length=1, max_length=256)
    client_tools: ClientToolsBody | None = None


class ActionBody(BaseModel):
    type: Literal["play_music"]
    action: Literal["play", "play_track", "skip", "stop", "queue"]
    station: str | None = None
    track: str | None = None


class ChatResponseBody(BaseModel):
    message: str
    conversation_id: str
    # For the caller to carry out; empty unless client_tools were sent and used.
    actions: list[ActionBody] = []


class ErrorBody(BaseModel):
    error: str


class KnowledgeStatusBody(BaseModel):
    documents: int
    chunks: int
    # Identity of the embedding model the index was built with.
    embedder: str
    indexed_at: datetime | None
    duration_ms: int

    @classmethod
    def of(cls, status: IndexStatus) -> "KnowledgeStatusBody":
        return cls(
            documents=status.documents,
            chunks=status.chunks,
            embedder=status.embedder,
            indexed_at=status.indexed_at,
            duration_ms=status.duration_ms,
        )


class DocumentBody(BaseModel):
    id: int
    namespace: str
    source: str
    title: str | None
    # pending_upload | queued | processing | ready | failed
    status: str
    # Detected from the content, not the uploader's claim.
    mime_type: str | None
    size_bytes: int | None
    content_hash: str | None
    error: str | None
    # Live (retrievable) chunks; a failed re-upload keeps the previous version's.
    chunks: int
    created_at: datetime
    updated_at: datetime
    processed_at: datetime | None

    @classmethod
    def of(cls, d: StoredDocument) -> "DocumentBody":
        return cls(
            id=d.id,
            namespace=d.namespace,
            source=d.source,
            title=d.title,
            status=d.status.value,
            mime_type=d.mime_type,
            size_bytes=d.size_bytes,
            content_hash=d.content_hash,
            error=d.error,
            chunks=d.chunks,
            created_at=d.created_at,
            updated_at=d.updated_at,
            processed_at=d.processed_at,
        )


class UploadRequestBody(BaseModel):
    namespace: str = Field(pattern=NAMESPACE_PATTERN)
    # Only its extension (.md, .markdown, .txt) and base name are used.
    filename: str = Field(min_length=1, max_length=255)
    size_bytes: int = Field(gt=0)
    # Identifies the document in its namespace; uploading it again replaces it.
    source: str | None = Field(default=None, min_length=1, max_length=2000)


class PresignedPostBody(BaseModel):
    method: Literal["POST"] = "POST"
    url: str
    # Send as multipart/form-data fields, then the file as the last field, named "file".
    fields: dict[str, str]


class UploadResponseBody(BaseModel):
    document: DocumentBody
    upload: PresignedPostBody
    expires_at: datetime
    max_bytes: int


class DocumentListBody(BaseModel):
    documents: list[DocumentBody]


class ApiError(Exception):
    def __init__(self, status_code: int, message: str, headers: dict[str, str] | None = None):
        self.status_code = status_code
        self.message = message
        self.headers = headers


_bearer = HTTPBearer(auto_error=False)


def _require_api_key(api_key: str):
    expected = api_key.encode()

    async def check(
        credentials: Annotated[HTTPAuthorizationCredentials | None, Depends(_bearer)],
    ) -> None:
        if credentials is None or not secrets.compare_digest(
            credentials.credentials.encode(), expected
        ):
            raise ApiError(401, "invalid or missing API key", {"WWW-Authenticate": "Bearer"})

    return check


def build_router(
    chat_service: ChatService,
    model: ModelPort,
    api_key: str,
    knowledge: KnowledgeBase | None = None,
    uploads: Uploads | None = None,
) -> APIRouter:
    router = APIRouter()
    authorized = [Depends(_require_api_key(api_key))]

    def knowledge_base() -> KnowledgeBase:
        if knowledge is None:
            raise ApiError(404, "knowledge base disabled")
        return knowledge

    def upload_service() -> Uploads:
        if uploads is None:
            raise ApiError(404, "uploads disabled")
        return uploads

    namespace_query = Query(pattern=NAMESPACE_PATTERN)
    document_errors = {code: {"model": ErrorBody} for code in (401, 404, 422, 503)}

    @router.post(
        "/v1/chat",
        response_model=ChatResponseBody,
        dependencies=[Depends(_require_api_key(api_key))],
        responses={code: {"model": ErrorBody} for code in (401, 422, 502, 503, 504)},
    )
    async def chat(body: ChatRequestBody) -> ChatResponseBody:
        tools = body.client_tools.to_domain() if body.client_tools else None
        reply = await chat_service.reply(body.conversation_id, body.message, tools)
        return ChatResponseBody(
            message=reply.message,
            conversation_id=body.conversation_id,
            actions=[
                ActionBody(
                    type="play_music", action=a.action.value, station=a.station, track=a.track
                )
                for a in reply.actions
            ],
        )

    @router.get(
        "/v1/knowledge",
        response_model=KnowledgeStatusBody,
        dependencies=authorized,
        responses={code: {"model": ErrorBody} for code in (401, 404)},
    )
    async def knowledge_status() -> KnowledgeStatusBody:
        """What is indexed now; `indexed_at` is null until the first index is built."""
        return KnowledgeStatusBody.of(knowledge_base().status)

    @router.post(
        "/v1/knowledge/reload",
        response_model=KnowledgeStatusBody,
        dependencies=authorized,
        responses={code: {"model": ErrorBody} for code in (401, 404, 503)},
    )
    async def knowledge_reload() -> KnowledgeStatusBody:
        """Re-read the documents and re-index; unchanged chunks come from the cache."""
        return KnowledgeStatusBody.of(await knowledge_base().refresh())

    @router.post(
        "/v1/knowledge/uploads",
        status_code=201,
        response_model=UploadResponseBody,
        dependencies=authorized,
        responses=document_errors,
    )
    async def create_upload(body: UploadRequestBody) -> UploadResponseBody:
        """Register a document and get a presigned POST to upload its file to S3 directly.
        The file is ingested asynchronously; poll the document for its status."""
        service = upload_service()
        document, upload = await service.create(
            body.namespace, body.filename, body.size_bytes, body.source
        )
        return UploadResponseBody(
            document=DocumentBody.of(document),
            upload=PresignedPostBody(url=upload.url, fields=upload.fields),
            expires_at=upload.expires_at,
            max_bytes=service.settings.max_bytes,
        )

    @router.get(
        "/v1/knowledge/documents",
        response_model=DocumentListBody,
        dependencies=authorized,
        responses=document_errors,
    )
    async def list_documents(namespace: str = namespace_query) -> DocumentListBody:
        """The namespace's documents, most recently changed first (at most 100)."""
        documents = await upload_service().list_documents(namespace)
        return DocumentListBody(documents=[DocumentBody.of(d) for d in documents])

    @router.get(
        "/v1/knowledge/documents/{document_id}",
        response_model=DocumentBody,
        dependencies=authorized,
        responses=document_errors,
    )
    async def get_document(document_id: int, namespace: str = namespace_query) -> DocumentBody:
        document = await upload_service().get(namespace, document_id)
        if document is None:
            raise ApiError(404, "document not found")
        return DocumentBody.of(document)

    @router.post(
        "/v1/knowledge/documents/{document_id}/reprocess",
        status_code=202,
        dependencies=authorized,
        responses=document_errors,
    )
    async def reprocess_document(
        document_id: int, namespace: str = namespace_query, force: bool = False
    ) -> Response:
        """Re-ingest after a pipeline change (embedding model, chunking); `force` also
        retries a failed or finished ingestion."""
        if not await upload_service().reprocess(namespace, document_id, force):
            raise ApiError(404, "document not found")
        return Response(status_code=202)

    @router.delete(
        "/v1/knowledge/documents/{document_id}",
        status_code=204,
        dependencies=authorized,
        responses=document_errors,
    )
    async def delete_document(document_id: int, namespace: str = namespace_query) -> Response:
        """Delete the document and its chunks (retrieval stops at once), then its file."""
        if not await upload_service().delete(namespace, document_id):
            raise ApiError(404, "document not found")
        return Response(status_code=204)

    @router.get("/health")
    async def health() -> dict[str, str]:
        """Liveness: the process is up and serving HTTP."""
        return {"status": "ok"}

    @router.get("/ready")
    async def ready() -> JSONResponse:
        """Readiness: the model backend is reachable and serves the model."""
        model_ready = await model.is_ready()
        return JSONResponse(
            status_code=200 if model_ready else 503,
            content={"status": "ready" if model_ready else "not_ready", "model": model_ready},
        )

    return router


# Model failures -> HTTP status. Messages are fixed strings: nothing from
# upstream bodies, URLs or credentials reaches the client.
_MODEL_ERRORS: list[tuple[type[ModelError], int, str]] = [
    (ModelTimeoutError, 504, "model timed out"),
    (ModelUnavailableError, 503, "model unavailable"),
    (ModelRequestRejectedError, 502, "model rejected the request"),
    (ModelResponseError, 502, "invalid model response"),
]


def install_error_handlers(app: FastAPI) -> None:
    @app.exception_handler(ApiError)
    async def api_error(_: Request, exc: ApiError) -> JSONResponse:
        return JSONResponse({"error": exc.message}, exc.status_code, headers=exc.headers)

    @app.exception_handler(RequestValidationError)
    async def validation_error(_: Request, exc: RequestValidationError) -> JSONResponse:
        problems = "; ".join(
            f"{'.'.join(str(p) for p in err['loc'][1:]) or 'body'}: {err['msg']}"
            for err in exc.errors()
        )
        return JSONResponse({"error": f"invalid request: {problems}"}, 422)

    @app.exception_handler(InvalidInputError)
    async def invalid_input(_: Request, exc: InvalidInputError) -> JSONResponse:
        return JSONResponse({"error": str(exc)}, 422)

    @app.exception_handler(EmbeddingError)
    async def embedding_error(_: Request, exc: EmbeddingError) -> JSONResponse:
        log.warning("embedding error: %s", exc)
        return JSONResponse({"error": "embedding unavailable"}, 503)

    @app.exception_handler(StorageError)
    async def storage_error(_: Request, exc: StorageError) -> JSONResponse:
        log.warning("storage error: %s", exc)
        return JSONResponse({"error": "storage unavailable"}, 503)

    @app.exception_handler(ModelError)
    async def model_error(_: Request, exc: ModelError) -> JSONResponse:
        for kind, status, message in _MODEL_ERRORS:
            if isinstance(exc, kind):
                log.warning("model error: %s", exc)
                return JSONResponse({"error": message}, status)
        log.error("unhandled model error: %r", exc)
        return JSONResponse({"error": "model error"}, 502)

    @app.exception_handler(Exception)
    async def unexpected(_: Request, exc: Exception) -> JSONResponse:
        log.exception("unhandled error", exc_info=exc)
        return JSONResponse({"error": "internal error"}, 500)
