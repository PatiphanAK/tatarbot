#!/usr/bin/env python3
"""Minimal stand-in for the Agent Runtime: echoes messages back.

POST /v1/chat {"message": "...", "conversation_id": "..."}
-> 200 {"message": "echo: ...", "conversation_id": "..."}

If MOCK_AGENT_API_KEY is set, requests must send `Authorization: Bearer <key>`.
"""
import json
import os
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

API_KEY = os.environ.get("MOCK_AGENT_API_KEY") or None


class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        if self.path.rstrip("/") != "/v1/chat":
            return self._send(404, {"error": "not found"})
        if API_KEY and self.headers.get("Authorization") != f"Bearer {API_KEY}":
            return self._send(401, {"error": "invalid or missing API key"})
        try:
            length = int(self.headers.get("Content-Length", 0))
            body = json.loads(self.rfile.read(length))
            message, conversation_id = body["message"], body["conversation_id"]
        except (ValueError, KeyError, TypeError):
            return self._send(400, {"error": "expected {message, conversation_id}"})
        self._send(200, {"message": f"echo: {message}", "conversation_id": conversation_id})

    def _send(self, status, payload):
        data = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8080
    auth = "on" if API_KEY else "off"
    print(f"mock agent runtime on http://127.0.0.1:{port}/v1/chat (auth {auth})", flush=True)
    ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
