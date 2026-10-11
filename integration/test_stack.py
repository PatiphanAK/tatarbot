#!/usr/bin/env python3
"""Black-box tests of the Agent Runtime container against the mock model.

Runs inside a container on the Compose network and reaches the runtime by its service name,
exactly as the Discord bot does. Standard library only.

    python test_stack.py up     # mock model running
    python test_stack.py down   # mock model stopped
"""

import json
import os
import sys
import urllib.error
import urllib.request

BASE = os.environ.get("AGENT_BASE_URL", "http://agent_runtime:8080").rstrip("/")
KEY = os.environ["AGENT_API_KEY"]
FAILURES: list[str] = []


def call(method, path, body=None, key=KEY, timeout=30):
    req = urllib.request.Request(BASE + path, method=method)
    data = None
    if body is not None:
        data = json.dumps(body).encode()
        req.add_header("Content-Type", "application/json")
    if key:
        req.add_header("Authorization", f"Bearer {key}")
    try:
        with urllib.request.urlopen(req, data, timeout=timeout) as resp:
            return resp.status, json.loads(resp.read() or b"null")
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read() or b"null")


def chat(message, conversation_id="it:conv", key=KEY):
    return call("POST", "/v1/chat", {"message": message, "conversation_id": conversation_id}, key)


def check(name, condition, detail=""):
    print(f"{'PASS' if condition else 'FAIL'}  {name}" + (f"  ({detail})" if detail else ""))
    if not condition:
        FAILURES.append(name)


def no_leak(body):
    text = json.dumps(body)
    return "mock_model" not in text and "secret" not in text and "mock-model-key" not in text


def backend_up():
    status, body = call("GET", "/health", key=None)
    check("health is public and ok", status == 200 and body == {"status": "ok"}, body)

    status, body = call("GET", "/ready", key=None)
    check("ready when model is served", status == 200 and body.get("model") is True, body)

    status, body = chat("hello", key=None)
    check("chat without key is 401", status == 401, body)
    status, body = chat("hello", key="wrong")
    check("chat with wrong key is 401", status == 401, body)

    status, body = chat("hello")
    reply = (body or {}).get("message", "")
    check("chat returns 200", status == 200, body)
    check("reply comes from the model", reply == "mock reply #1: hello", reply)
    check("reasoning is stripped", "<think>" not in reply and "hidden" not in reply, reply)
    check("conversation_id echoed", (body or {}).get("conversation_id") == "it:conv", body)

    status, body = chat("again")
    check("history is sent on the second turn", body.get("message") == "mock reply #2: again", body)

    status, body = chat("fresh", conversation_id="it:other")
    check("conversations are isolated", body.get("message") == "mock reply #1: fresh", body)

    status, body = chat("   ")
    check("blank message is 422", status == 422, body)

    status, body = chat("__fail_500__", conversation_id="it:fail")
    check("model 5xx -> 503", status == 503 and body == {"error": "model unavailable"}, body)
    check("model 5xx body not leaked", no_leak(body), body)

    status, body = chat("__bad_json__", conversation_id="it:bad")
    check("non-OpenAI 200 body -> 502", status == 502, body)

    status, body = chat("__slow__", conversation_id="it:slow")
    check("model timeout -> 504", status == 504 and body == {"error": "model timed out"}, body)

    # Client-side tools: the contract the Discord bot relies on.
    stations = [{"name": "lofi", "title": "Lo-fi"}, {"name": "jazz", "title": "Jazz"}]
    status, body = call(
        "POST",
        "/v1/chat",
        {
            "message": "__play_lofi__",
            "conversation_id": "it:music",
            "client_tools": {"music": {"stations": stations}},
        },
    )
    check(
        "music request returns an action, not text",
        status == 200
        and body.get("actions") == [{"type": "play_music", "action": "play", "station": "lofi"}]
        and body.get("message") == "",
        body,
    )
    # The next turn reports the outcome and the current state, as the bot does.
    status, body = call(
        "POST",
        "/v1/chat",
        {
            "message": "what's playing?",
            "conversation_id": "it:music",
            "client_tools": {
                "music": {
                    "stations": stations,
                    "state": {
                        "channel": "General",
                        "queue": [{"title": "Lo-fi", "live": True, "song": "A - B"}],
                    },
                    "results": [
                        {"action": "play", "station": "lofi", "ok": True, "detail": "Now playing"}
                    ],
                }
            },
        },
    )
    check(
        "outcomes and playback state are accepted",
        status == 200 and body.get("actions") == [] and "#2" in body.get("message", ""),
        body,
    )
    status, body = chat("__play_lofi__", conversation_id="it:no-tools")
    check(
        "no client_tools: no tool offered, no actions",
        status == 200 and body.get("actions") == [] and "__play_lofi__" in body.get("message", ""),
        body,
    )

    status, body = chat("after failures", conversation_id="it:fail")
    check(
        "failed turns are not stored", body.get("message") == "mock reply #1: after failures", body
    )


def backend_down():
    status, body = call("GET", "/health", key=None)
    check("health stays ok without the model", status == 200, body)
    status, body = call("GET", "/ready", key=None)
    check("not ready without the model", status == 503 and body.get("model") is False, body)
    status, body = chat("hello", conversation_id="it:down")
    check(
        "model unreachable -> 503", status == 503 and body == {"error": "model unavailable"}, body
    )
    check("unreachable error not leaked", no_leak(body), body)


if __name__ == "__main__":
    {"up": backend_up, "down": backend_down}[sys.argv[1]]()
    if FAILURES:
        print(f"\n{len(FAILURES)} check(s) failed")
        sys.exit(1)
    print("\nall checks passed")
