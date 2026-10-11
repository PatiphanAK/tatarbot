"""Client-side tools: offered to the model, carried out by the caller (the bot).

History: a turn that requested actions is stored the way models understand tool use: an
assistant message with the `play_music` calls, then one tool message per call, "pending"
until the caller reports the outcome with its next request ("ok: Now playing ..." or
"failed: ..."). Storing them as text notes made a model imitate the notes instead of
calling the tool. The caller also sends the current playback state each turn, which goes
into the system message.

The runtime never runs these. The bot says which it supports in the request (today only
music, with its station list); if the model calls one, the runtime validates the arguments
and returns them as actions, and the bot executes them through its own rules (e.g. the
user must be in a voice channel). The arguments are enums built from what the bot sent, so
even a successful prompt injection can only pick a listed station or action.
"""

import json
from dataclasses import dataclass, field
from enum import StrEnum

from agent_runtime.domain.inference import ToolCall, ToolSpec

PLAY_MUSIC = "play_music"


class MusicAction(StrEnum):
    PLAY = "play"
    # A song from the server's uploaded library, by title (free text).
    PLAY_TRACK = "play_track"
    SKIP = "skip"
    STOP = "stop"
    QUEUE = "queue"


MAX_TRACK_CHARS = 100
MAX_HINTS = 20


@dataclass(frozen=True, slots=True)
class Station:
    name: str
    title: str


@dataclass(frozen=True, slots=True)
class PlayingTrack:
    title: str
    live: bool = False
    # A live station's current song (ICY title).
    song: str | None = None


@dataclass(frozen=True, slots=True)
class MusicState:
    """What the bot is doing now in this server, as reported by the bot."""

    channel: str | None = None
    queue: tuple[PlayingTrack, ...] = ()


@dataclass(frozen=True, slots=True)
class ActionResult:
    """The outcome of an action the bot carried out after an earlier turn."""

    action: "MusicAction"
    station: str | None
    ok: bool
    detail: str = ""
    track: str | None = None


@dataclass(frozen=True, slots=True)
class MusicTool:
    stations: tuple[Station, ...]
    state: MusicState | None = None
    results: tuple[ActionResult, ...] = ()
    # The server has an uploaded library the bot can search by title.
    library: bool = False
    # Recent library titles, to help match a request; not a complete list.
    library_hints: tuple[str, ...] = ()


@dataclass(frozen=True, slots=True)
class ClientTools:
    music: MusicTool | None = None

    def specs(self) -> tuple[ToolSpec, ...]:
        return (play_music_spec(self.music),) if self.music else ()


@dataclass(frozen=True, slots=True)
class ClientAction:
    """A validated client-side tool call, returned to the caller."""

    action: MusicAction
    station: str | None = None
    type: str = PLAY_MUSIC
    # The model's tool call id, to pair the call with its result in history.
    call_id: str = field(default="", compare=False)
    # play_track only: the title to search for in the server's library.
    track: str | None = None

    def as_tool_call(self) -> ToolCall:
        args = {"action": self.action.value}
        if self.station:
            args["station"] = self.station
        if self.track:
            args["track"] = self.track
        return ToolCall(self.call_id or "call", PLAY_MUSIC, json.dumps(args))


def play_music_spec(tool: MusicTool) -> ToolSpec:
    listing = "; ".join(f"{s.name}: {s.title}" for s in tool.stations)
    actions = [a.value for a in MusicAction if tool.library or a is not MusicAction.PLAY_TRACK]
    properties: dict = {
        "action": {"type": "string", "enum": actions},
        "station": {
            "type": "string",
            "enum": [s.name for s in tool.stations],
            "description": "Required for action=play; the station that best fits.",
        },
    }
    library = ""
    if tool.library:
        hints = "; ".join(clean(t, MAX_TRACK_CHARS) for t in tool.library_hints[:MAX_HINTS])
        properties["track"] = {
            "type": "string",
            "maxLength": MAX_TRACK_CHARS,
            "description": (
                "Required for action=play_track: the song's title (or part of it) to look up "
                "in this server's uploaded library. Titles only, never links."
                + (f" Recent titles (not a complete list): {hints}." if hints else "")
            ),
        }
        library = (
            " play_track plays or queues a song from this server's uploaded library, found "
            "by title."
        )
    return ToolSpec(
        name=PLAY_MUSIC,
        description=(
            "Control music in the user's Discord voice channel: play one of the bot's radio "
            "stations, skip the current track, stop, or show the queue." + library + " Use "
            "only when the user asks for music. The bot carries it out and reports the result, "
            f"so don't claim success yourself. Stations: {listing}."
        ),
        parameters={"type": "object", "properties": properties, "required": ["action"]},
    )


def parse_play_music(call: ToolCall, tool: MusicTool) -> ClientAction | None:
    """The action a `play_music` call asks for, or None if its arguments are invalid."""
    if call.name != PLAY_MUSIC:
        return None
    try:
        args = json.loads(call.arguments)
        action = MusicAction(str(args["action"]).strip().lower())
    except (ValueError, KeyError, TypeError):
        return None
    if action is MusicAction.PLAY_TRACK:
        if not tool.library:
            return None
        track = clean(str(args.get("track") or ""), MAX_TRACK_CHARS)
        # The bot also refuses links; the runtime drops them first.
        if not track or "://" in track:
            return None
        return ClientAction(action, call_id=call.id, track=track)
    if action is not MusicAction.PLAY:
        return ClientAction(action, call_id=call.id)
    station = str(args.get("station") or "").strip().lower()
    if station not in {s.name for s in tool.stations}:
        return None
    return ClientAction(action, station, call_id=call.id)


MAX_DATA_CHARS = 120


def clean(text: str, limit: int = MAX_DATA_CHARS) -> str:
    """Titles from radio streams and uploads are untrusted: one line, bounded."""
    flat = " ".join("".join(c if c.isprintable() else " " for c in text).split())
    return flat if len(flat) <= limit else flat[: limit - 1] + "…"


PENDING = "pending: the bot is carrying this out"


def result_text(result: ActionResult) -> str:
    detail = clean(result.detail) or ("done" if result.ok else "no reason given")
    return f"{'ok' if result.ok else 'failed'}: {detail}"


def apply_results(
    calls: tuple[ToolCall, ...], tool_messages: list[str], results: tuple[ActionResult, ...]
) -> list[str]:
    """New contents for the pending tool messages of `calls`, matched to the reported
    results by action and station, in order. Unmatched messages stay as they were."""
    contents = list(tool_messages)
    used: set[int] = set()
    for result in results:
        for i, call in enumerate(calls):
            if i in used or i >= len(contents) or contents[i] != PENDING:
                continue
            try:
                args = json.loads(call.arguments)
            except ValueError:
                continue
            if (
                args.get("action") == result.action.value
                and args.get("station") == result.station
                and args.get("track") == result.track
            ):
                contents[i] = result_text(result)
                used.add(i)
                break
    return contents


def describe_state(state: MusicState | None) -> str:
    """The playback section of the system message (data from the bot)."""
    if state is None:
        return "- Unknown."
    if state.channel is None:
        return "- The bot is not in a voice channel; nothing is playing."
    lines = [f"- Voice channel: #{clean(state.channel, 60)}"]
    if not state.queue:
        lines.append("- Nothing is playing.")
        return "\n".join(lines)
    now, rest = state.queue[0], state.queue[1:]
    kind = "live radio" if now.live else "track"
    song = f"; current song: {clean(now.song)}" if now.song else ""
    lines.append(f"- Playing: {clean(now.title)} ({kind}{song})")
    if rest:
        lines.append(f"- Queued after it: {len(rest)}")
    return "\n".join(lines)
