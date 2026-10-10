#!/usr/bin/env python3
"""Smoke-test an OpenAI-compatible model endpoint the way the Agent Runtime calls it.

Reads the runtime's own variables, so it can run against agent_runtime/.env unchanged:
MODEL_BASE_URL (API root, e.g. http://127.0.0.1:8000/v1), MODEL_NAME, MODEL_API_KEY,
MODEL_MAX_TOKENS (1024), MODEL_TEMPERATURE (unset), MODEL_ENABLE_THINKING (false; empty
omits chat_template_kwargs). SMOKE_TIMEOUT_SECONDS (300) bounds each request so a cold
start can be measured instead of cut off at the runtime's 100 s.

Checks, in order:
  1. POST /chat/completions with the runtime's exact body returns 200 and non-empty text.
     Run twice: the first call includes any cold start, the second is warm. Chat runs
     first so a scale-to-zero endpoint is not woken by the /models call before it.
  2. GET  /models lists MODEL_NAME (what the runtime's /ready checks, with a 3 s timeout).
  3. POST /chat/completions without the key is rejected (401/403), if a key is set.
  4. POST /chat/completions with the runtime's web_search tool returns 200 (the server has
     tool calling enabled) and reports whether the model chose to call it. Skipped with
     SMOKE_TOOLS=false.

Prints latencies and token usage. Never prints the key or response bodies of failures
beyond a short prefix. Exits non-zero on the first failed check. Standard library only.
"""

import json
import os
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

# The runtime's packaged default prompt, when run from this repository.
PROMPT_FILE = (
    Path(__file__).resolve().parents[2] / "agent_runtime/src/agent_runtime/prompts/system.md"
)
FALLBACK_SYSTEM_PROMPT = "You are a helpful, friendly assistant. Answer concisely and accurately."
PROMPT = "Say hello in one short sentence."


def env(key: str, default: str | None = None) -> str | None:
    value = os.environ.get(key, "").strip()
    return value or default


def required(key: str) -> str:
    value = env(key)
    if value is None:
        sys.exit(f"FAIL: missing environment variable {key}")
    return value


BASE_URL = required("MODEL_BASE_URL").rstrip("/")
MODEL = required("MODEL_NAME")
API_KEY = env("MODEL_API_KEY")
TIMEOUT = float(env("SMOKE_TIMEOUT_SECONDS", "300"))


def request(method: str, path: str, body: dict | None = None, *, auth: bool = True):
    """Return (status, parsed JSON or None, raw text, seconds)."""
    headers = {"Content-Type": "application/json"}
    if auth and API_KEY:
        headers["Authorization"] = f"Bearer {API_KEY}"
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(f"{BASE_URL}/{path}", data=data, headers=headers, method=method)
    start = time.monotonic()
    try:
        with urllib.request.urlopen(req, timeout=TIMEOUT) as resp:
            status, raw = resp.status, resp.read().decode(errors="replace")
    except urllib.error.HTTPError as e:
        status, raw = e.code, e.read().decode(errors="replace")
    except (urllib.error.URLError, TimeoutError, OSError) as e:
        reason = getattr(e, "reason", e)
        sys.exit(f"FAIL: {method} /{path}: {type(reason).__name__}: {reason}")
    elapsed = time.monotonic() - start
    try:
        parsed = json.loads(raw)
    except ValueError:
        parsed = None
    return status, parsed, raw, elapsed


def system_prompt() -> str:
    """Base prompt + the English reply directive the runtime adds for PROMPT."""
    if inline := env("SYSTEM_PROMPT"):
        base = inline
    else:
        path = Path(env("SYSTEM_PROMPT_FILE") or PROMPT_FILE)
        base = path.read_text("utf-8").strip() if path.is_file() else FALLBACK_SYSTEM_PROMPT
    return f"{base}\n\nThe user is writing in English. Reply in English."


def chat_body() -> dict:
    """Mirror agent_runtime.adapters.outbound.vllm.VLLMModel._to_body."""
    body = {
        "model": MODEL,
        "messages": [
            {"role": "system", "content": system_prompt()},
            {"role": "user", "content": PROMPT},
        ],
        "stream": False,
    }
    max_tokens = env("MODEL_MAX_TOKENS", "1024")
    body["max_tokens"] = int(max_tokens)
    if (temperature := env("MODEL_TEMPERATURE")) is not None:
        body["temperature"] = float(temperature)
    thinking = os.environ.get("MODEL_ENABLE_THINKING", "false").strip().lower()
    if thinking:
        body["chat_template_kwargs"] = {"enable_thinking": thinking in ("1", "true", "yes", "on")}
    return body


WEB_SEARCH_TOOL = {
    "type": "function",
    "function": {
        "name": "web_search",
        "description": "Search the web. Use for current events, news, prices, schedules, "
        "recent releases, or facts you are not sure about.",
        "parameters": {
            "type": "object",
            "properties": {"query": {"type": "string"}},
            "required": ["query"],
        },
    },
}


def fail(check: str, status: int, raw: str) -> None:
    sys.exit(f"FAIL: {check}: HTTP {status}: {raw[:300]}")


def check_models() -> None:
    status, data, raw, elapsed = request("GET", "models")
    if status != 200 or not isinstance(data, dict):
        fail("GET /models", status, raw)
    served = [m.get("id") for m in data.get("data", []) if isinstance(m, dict)]
    if MODEL not in served:
        sys.exit(f"FAIL: GET /models: {MODEL!r} not served (served: {served})")
    print(f"ok   GET /models lists {MODEL!r} ({elapsed:.1f}s)")
    if elapsed > 3:
        print("warn /models took over 3 s; the runtime's /ready would report not_ready")


def check_auth_required() -> None:
    if not API_KEY:
        print("skip unauthenticated request (MODEL_API_KEY not set)")
        return
    status, _, raw, _ = request("POST", "chat/completions", chat_body(), auth=False)
    if status not in (401, 403):
        fail("POST /chat/completions without key should be rejected", status, raw)
    print(f"ok   request without key rejected (HTTP {status})")


def check_chat(label: str) -> None:
    status, data, raw, elapsed = request("POST", "chat/completions", chat_body())
    if status != 200:
        fail(f"POST /chat/completions ({label})", status, raw)
    try:
        choice = data["choices"][0]
        content = choice["message"]["content"]
    except (KeyError, IndexError, TypeError):
        fail(f"POST /chat/completions ({label}): not an OpenAI response", status, raw)
    if not isinstance(content, str) or not content.strip():
        fail(f"POST /chat/completions ({label}): empty content", status, raw)
    usage = data.get("usage") or {}
    print(
        f"ok   chat ({label}) {elapsed:.1f}s, finish={choice.get('finish_reason')}, "
        f"tokens={usage.get('prompt_tokens')}+{usage.get('completion_tokens')}"
    )
    if "<think>" in content:
        print("warn reply contains <think>; the runtime strips it, but it costs tokens")
    print(f"     reply: {content.strip()[:200]!r}")


def check_tools() -> None:
    if env("SMOKE_TOOLS", "true").lower() in ("0", "false", "no", "off"):
        print("skip tool calling (SMOKE_TOOLS=false)")
        return
    body = chat_body()
    body["messages"][-1]["content"] = "What is the latest news about Thailand today?"
    body["tools"] = [WEB_SEARCH_TOOL]
    body["tool_choice"] = "auto"
    status, data, raw, elapsed = request("POST", "chat/completions", body)
    if status != 200:
        print("hint: vLLM needs --enable-auto-tool-choice --tool-call-parser hermes")
        fail("POST /chat/completions with tools", status, raw)
    try:
        calls = data["choices"][0]["message"].get("tool_calls") or []
    except (KeyError, IndexError, TypeError):
        fail("POST /chat/completions with tools: not an OpenAI response", status, raw)
    if calls:
        fn = calls[0].get("function", {})
        print(f"ok   tool call {fn.get('name')}({fn.get('arguments')}) ({elapsed:.1f}s)")
    else:
        print(f"warn tools accepted but the model answered without searching ({elapsed:.1f}s)")


if __name__ == "__main__":
    print(f"endpoint {BASE_URL}, model {MODEL!r}, timeout {TIMEOUT:.0f}s")
    check_chat("first, may be cold")
    check_chat("warm")
    check_models()
    check_auth_required()
    check_tools()
    print("all checks passed")
