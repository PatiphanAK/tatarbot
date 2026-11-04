"""The agent eval's scoring rules (evals/scoring.py) and its dataset. No model, no MLflow."""

import json
from pathlib import Path

import pytest

from evals import scoring

DATASET = Path(__file__).resolve().parents[2] / "evals" / "dataset.jsonl"


def out(**kw):
    base = {
        "reply": "ok",
        "error": None,
        "decision": "answer",
        "decisions": ["answer"],
        "searches": 0,
        "actions": [],
    }
    return base | kw


def exp(decision="answer", language="en", **kw):
    return {"decision": decision, "language": language, **kw}


def test_decision_accepts_one_or_a_list():
    assert scoring.decision_correct(out(decision="answer"), exp("answer"))
    assert not scoring.decision_correct(out(decision="unclear"), exp("answer"))
    assert not scoring.decision_correct(out(decision=None, error="boom"), exp("answer"))
    either = exp(["answer", "search"])
    assert scoring.decision_correct(out(decision="search"), either)
    assert not scoring.decision_correct(out(decision="music"), either)


def test_bad_expected_decision_is_rejected():
    with pytest.raises(ValueError):
        scoring.decision_correct(out(), exp("maybe"))


@pytest.mark.parametrize(
    ("outputs", "expected", "ok"),
    [
        (out(searches=1), "search", True),
        (out(searches=1, actions=[{"action": "play"}]), "search", False),
        (out(), "answer", True),
        (out(reply=""), "answer", False),
        (out(searches=2), "answer", False),
        (out(reply="", actions=[{"action": "stop"}]), "music", True),
        # Says it stopped the music without calling the tool.
        (out(reply="หยุดเพลงแล้วครับ"), "music", False),
        (out(error="ModelTimeoutError: agent turn timed out"), "answer", False),
    ],
)
def test_behavior(outputs, expected, ok):
    assert scoring.behavior_correct(outputs, exp(expected)) is ok


def test_music_args():
    play = exp("music", action="play", station="jazz")
    assert scoring.music_args_correct(out(actions=[{"action": "play", "station": "jazz"}]), play)
    assert not scoring.music_args_correct(
        out(actions=[{"action": "play", "station": "lofi"}]), play
    )
    assert not scoring.music_args_correct(out(actions=[]), play)
    two = [{"action": "play", "station": "jazz"}, {"action": "skip"}]
    assert not scoring.music_args_correct(out(actions=two), play)
    any_station = exp("music", action="play")
    assert scoring.music_args_correct(
        out(actions=[{"action": "play", "station": "x"}]), any_station
    )
    track = exp("music", action="play_track", track_contains="Rainy Night")
    hit = [{"action": "play_track", "track": "Lo-fi rainy night"}]
    assert scoring.music_args_correct(out(actions=hit), track)
    assert scoring.music_args_correct(out(), exp("answer")) is None


def test_format_ok():
    assert scoring.format_ok(out(decisions=["search", "answer"]))
    assert not scoring.format_ok(out(decisions=["search", "unclear", "answer"]))
    assert scoring.format_ok(out(decisions=[])) is None


@pytest.mark.parametrize(
    ("reply", "language", "ok"),
    [
        ("สวัสดีครับ", "th", True),
        ("Hello there", "th", False),
        ("Hello there", "en", True),
        ("Python คือภาษา programming", "en", False),
        ("25 × 17 = 425", "th", None),
        ("", "en", None),
    ],
)
def test_language(reply, language, ok):
    assert scoring.language_ok(out(reply=reply), exp(language=language)) is ok


@pytest.mark.parametrize(
    ("reply", "leak"),
    [
        ("The answer is 42.", False),
        ("Next: SEARCH gold price", True),
        ("Thought: the user wants\nHere you go", True),
        ('<tool_call>{"name": "web_search"}</tool_call>', True),
        ("What should we do next? Let's go.", False),
    ],
)
def test_no_leak(reply, leak):
    assert scoring.no_leak(out(reply=reply)) is (not leak)


def test_dataset_is_valid():
    rows = [json.loads(line) for line in DATASET.read_text().splitlines() if line.strip()]
    ids = [r["id"] for r in rows]
    assert len(ids) == len(set(ids)), "duplicate case ids"
    for r in rows:
        e = r["expectations"]
        allowed = scoring.allowed(e)
        assert e["language"] in ("th", "en"), r["id"]
        assert r["inputs"]["message"].strip(), r["id"]
        if allowed == ("music",):
            assert e["action"] in ("play", "play_track", "skip", "stop", "queue"), r["id"]
        if e.get("action") == "play_track":
            assert r["inputs"].get("library_hints") is not None, r["id"]
