"""Inbound HTTP adapter: translates HTTP to `ChatService` calls.

Wire contract (unchanged from TatarBot's `AgentPort` HTTP adapter):

    POST /v1/chat  {"message": str, "conversation_id": str}
    200 {"message": str, "conversation_id": str}
    4xx/5xx {"error": str}
"""

import logging
import secrets
from typing import Annotated, Literal

from fastapi import APIRouter, Depends, FastAPI, Request
from fastapi.exceptions import RequestValidationError
from fastapi.responses import JSONResponse
from fastapi.security import HTTPAuthorizationCredentials, HTTPBearer
from pydantic import BaseModel, Field, field_validator

from agent_runtime.application import ChatService, ModelPort
from agent_runtime.domain import (
    InvalidInputError,
    ModelError,
    ModelRequestRejectedError,
    ModelResponseError,
    ModelTimeoutError,
    ModelUnavailableError,
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

    action: Literal["play", "skip", "stop", "queue"]
    station: str | None = Field(default=None, max_length=32)
    ok: bool
    detail: str = Field(default="", max_length=300)


class MusicToolBody(BaseModel):
    stations: list[StationBody] = Field(min_length=1, max_length=25)
    state: MusicStateBody | None = None
    results: list[ActionResultBody] = Field(default_factory=list, max_length=4)

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
            ActionResult(MusicAction(r.action), r.station, r.ok, r.detail) for r in music.results
        )
        return ClientTools(music=MusicTool(stations, state, results))


class ChatRequestBody(BaseModel):
    message: str = Field(min_length=1, max_length=MAX_MESSAGE_LENGTH)
    conversation_id: str = Field(min_length=1, max_length=256)
    client_tools: ClientToolsBody | None = None


class ActionBody(BaseModel):
    type: Literal["play_music"]
    action: Literal["play", "skip", "stop", "queue"]
    station: str | None = None


class ChatResponseBody(BaseModel):
    message: str
    conversation_id: str
    # For the caller to carry out; empty unless client_tools were sent and used.
    actions: list[ActionBody] = []


class ErrorBody(BaseModel):
    error: str


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


def build_router(chat_service: ChatService, model: ModelPort, api_key: str) -> APIRouter:
    router = APIRouter()

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
                ActionBody(type="play_music", action=a.action.value, station=a.station)
                for a in reply.actions
            ],
        )

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
