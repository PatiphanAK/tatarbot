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
    SKIP = "skip"
    STOP = "stop"
    QUEUE = "queue"


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


@dataclass(frozen=True, slots=True)
class MusicTool:
    stations: tuple[Station, ...]
    state: MusicState | None = None
    results: tuple[ActionResult, ...] = ()


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

    def as_tool_call(self) -> ToolCall:
        args = {"action": self.action.value}
        if self.station:
            args["station"] = self.station
        return ToolCall(self.call_id or "call", PLAY_MUSIC, json.dumps(args))


def play_music_spec(tool: MusicTool) -> ToolSpec:
    listing = "; ".join(f"{s.name}: {s.title}" for s in tool.stations)
    return ToolSpec(
        name=PLAY_MUSIC,
        description=(
            "Control music in the user's Discord voice channel: play one of the bot's radio "
            "stations, skip the current track, stop, or show the queue. Use only when the "
            "user asks for music or radio. The bot carries it out and reports the result, "
            f"so don't claim success yourself. Stations: {listing}."
        ),
        parameters={
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": [a.value for a in MusicAction]},
                "station": {
                    "type": "string",
                    "enum": [s.name for s in tool.stations],
                    "description": "Required for action=play; the station that best fits.",
                },
            },
            "required": ["action"],
        },
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
            if args.get("action") == result.action.value and args.get("station") == result.station:
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
