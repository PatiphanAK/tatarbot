#!/usr/bin/env python3
"""Minimal OpenAI-compatible Chat Completions backend for integration tests.

GET  /v1/models            -> {"data": [{"id": MOCK_MODEL_NAME}]}
POST /v1/chat/completions  -> a reply that echoes the last user message and counts the user
                              turns it received, wrapped with a <think> block the runtime
                              must strip.

Magic user messages trigger failures: "__fail_500__" (HTTP 500), "__bad_json__" (200 with a
non-OpenAI body), "__slow__" (sleeps MOCK_SLOW_SECONDS). "__play_lofi__" answers with a
play_music tool call if that tool was offered, else plain text. If MOCK_API_KEY is set, requests
must send `Authorization: Bearer <key>`. Standard library only.
"""

import json
import os
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

API_KEY = os.environ.get("MOCK_API_KEY") or None
MODEL = os.environ.get("MOCK_MODEL_NAME", "mock-model")
SLOW_SECONDS = float(os.environ.get("MOCK_SLOW_SECONDS", "10"))


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path.rstrip("/") != "/v1/models":
            return self._send(404, {"error": {"message": "not found"}})
        if not self._authorized():
            return self._send(401, {"error": {"message": "invalid api key"}})
        self._send(200, {"object": "list", "data": [{"id": MODEL, "object": "model"}]})

    def do_POST(self):
        if self.path.rstrip("/") != "/v1/chat/completions":
            return self._send(404, {"error": {"message": "not found"}})
        if not self._authorized():
            return self._send(401, {"error": {"message": "invalid api key"}})
        try:
            body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0))))
            messages = body["messages"]
            last_user = [m["content"] for m in messages if m["role"] == "user"][-1]
        except (ValueError, KeyError, IndexError, TypeError):
            return self._send(400, {"error": {"message": "bad request"}})
        if body.get("model") != MODEL:
            return self._send(404, {"error": {"message": f"model {body.get('model')!r} not found"}})
        if body.get("stream"):
            return self._send(400, {"error": {"message": "streaming not supported by mock"}})

        if last_user == "__fail_500__":
            return self._send(500, {"error": {"message": "internal secret detail"}})
        if last_user == "__bad_json__":
            return self._send(200, {"status": "IN_QUEUE"})
        if last_user == "__slow__":
            time.sleep(SLOW_SECONDS)
        if last_user == "__play_lofi__":
            offered = [t["function"]["name"] for t in body.get("tools", [])]
            if "play_music" in offered:
                call = {
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "play_music",
                        "arguments": json.dumps({"action": "play", "station": "lofi"}),
                    },
                }
                message = {"role": "assistant", "content": None, "tool_calls": [call]}
                return self._send(
                    200,
                    {
                        "id": "chatcmpl-mock",
                        "object": "chat.completion",
                        "model": MODEL,
                        "choices": [{"index": 0, "message": message, "finish_reason": "tool_calls"}],
                    },
                )

        user_turns = sum(1 for m in messages if m["role"] == "user")
        content = f"<think>hidden reasoning</think>mock reply #{user_turns}: {last_user}"
        self._send(
            200,
            {
                "id": "chatcmpl-mock",
                "object": "chat.completion",
                "model": MODEL,
                "choices": [
                    {
                        "index": 0,
                        "message": {"role": "assistant", "content": content},
                        "finish_reason": "stop",
                    }
                ],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
            },
        )

    def _authorized(self) -> bool:
        return API_KEY is None or self.headers.get("Authorization") == f"Bearer {API_KEY}"

    def _send(self, status, payload):
        data = json.dumps(payload).encode()
        try:
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
        except (BrokenPipeError, ConnectionResetError):
            pass  # the client gave up (timeout tests)

    def log_message(self, fmt, *args):
        sys.stderr.write(f"mock_openai: {fmt % args}\n")


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8000
    ThreadingHTTPServer(("0.0.0.0", port), Handler).serve_forever()
