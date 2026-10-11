"""Client-side tools: offered to the model, carried out by the caller (the bot).

The runtime never runs these. The bot says which it supports in the request (today only
music, with its station list); if the model calls one, the runtime validates the arguments
and returns them as actions, and the bot executes them through its own rules (e.g. the
user must be in a voice channel). The arguments are enums built from what the bot sent, so
even a successful prompt injection can only pick a listed station or action.
"""

import json
from dataclasses import dataclass
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
class MusicTool:
    stations: tuple[Station, ...]


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
        return ClientAction(action)
    station = str(args.get("station") or "").strip().lower()
    if station not in {s.name for s in tool.stations}:
        return None
    return ClientAction(action, station)


def describe(action: ClientAction) -> str:
    """Neutral history note for a turn that ended in an action."""
    return f"[music: {action.action}{' ' + action.station if action.station else ''}]"
