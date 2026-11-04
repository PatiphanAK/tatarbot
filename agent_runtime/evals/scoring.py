"""Scoring rules for the agent eval. Pure functions, no MLflow and no I/O, so the unit tests
run without the `eval` dependency group.

Each check takes the harness output of one case (`run.py` `Turn.as_dict()`) and the case's
expectations, and returns True/False, or None when the check does not apply to that case
(MLflow then leaves it out of the mean).

Expectations (`dataset.jsonl` `expectations`):
* `decision`: what the first reasoning step should choose: `search`, `answer` or `music`,
  or a list of the acceptable ones (e.g. `["answer", "search"]` for a fact the model may
  know or may check).
* `action` (music only): `play`, `play_track`, `skip`, `stop` or `queue`.
* `station` (action=play, optional): the station that should be chosen. Omitted when
  several stations fit.
* `track_contains` (action=play_track, optional): a substring of the requested title.
* `language`: `th` or `en`, the language the reply should be in.
"""

import re

from agent_runtime.domain.language import contains_thai, detect_language

DECISIONS = ("search", "answer", "music")

# Agent scaffolding that must never reach the user.
_LEAK = re.compile(
    r"^\W*(thought|next|action|observation)\W*:"
    r"|<tool_call>|\[agent step|Next:\s*(SEARCH|ANSWER|MUSIC)",
    re.IGNORECASE | re.MULTILINE,
)


def turn_ok(outputs: dict) -> bool:
    """The turn finished without an error (timeout, model error, empty reply)."""
    return not outputs.get("error")


def allowed(expectations: dict) -> tuple[str, ...]:
    """The acceptable decisions of a case."""
    decision = expectations["decision"]
    allowed = (decision,) if isinstance(decision, str) else tuple(decision)
    if not allowed or any(d not in DECISIONS for d in allowed):
        raise ValueError(f"bad expected decision {decision!r}")
    return allowed


def decision_correct(outputs: dict, expectations: dict) -> bool | None:
    """The first reasoning step chose one of the acceptable decisions."""
    return outputs.get("decision") in allowed(expectations)


def behavior_correct(outputs: dict, expectations: dict) -> bool | None:
    """The turn as a whole did the right kind of thing, whatever the first thought said:
    searched (and took no music action), answered without searching, or took a music
    action (without searching)."""
    if outputs.get("error"):
        return False
    searched = outputs.get("searches", 0) > 0
    acted = bool(outputs.get("actions"))
    did = {
        "search": searched and not acted,
        "answer": not searched and not acted and bool(outputs.get("reply")),
        "music": acted and not searched,
    }
    return any(did[d] for d in allowed(expectations))


def music_args_correct(outputs: dict, expectations: dict) -> bool | None:
    """Music-only cases: exactly one action, with the expected action, station and track."""
    if allowed(expectations) != ("music",):
        return None
    actions = outputs.get("actions") or []
    if len(actions) != 1:
        return False
    action = actions[0]
    if action.get("action") != expectations.get("action"):
        return False
    if (station := expectations.get("station")) and action.get("station") != station:
        return False
    track = expectations.get("track_contains")
    return not track or track.lower() in (action.get("track") or "").lower()


def format_ok(outputs: dict) -> bool | None:
    """Every reasoning step ended with a valid `Next:` line (no `unclear` decision)."""
    decisions = outputs.get("decisions") or []
    if not decisions:
        return None
    return all(d in DECISIONS for d in decisions)


def language_ok(outputs: dict, expectations: dict) -> bool | None:
    """The reply is in the expected language. Replies without letters (a music action
    only, or just numbers) don't count."""
    reply = outputs.get("reply") or ""
    if detect_language(reply) is None:
        return None  # no text, or no letters (e.g. "25 × 17 = 425")
    thai = contains_thai(reply)
    return thai if expectations["language"] == "th" else not thai


def no_leak(outputs: dict) -> bool | None:
    """The reply contains no agent scaffolding (thoughts, Next lines, tool-call markup)."""
    reply = outputs.get("reply") or ""
    if not reply:
        return None
    return not _LEAK.search(reply)
