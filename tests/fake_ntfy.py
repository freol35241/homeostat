# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""A minimal, honest ntfy server for the ntfy adapter's integration tests
(tests/ntfy.rs; see docs/design.md, "The ntfy adapter").

Speaks the two calls the adapter makes: GET /v1/health -> {"healthy":
true}, and POST /{topic} with the message as the body and Title /
Priority headers -> the message document ntfy returns ({id, time, topic,
message, title, priority}). Every publish must carry `Authorization:
Bearer <token>` matching --token — anything else is 401 — which is how
the tests know the adapter authenticates.

Test control rides plain HTTP on the same port:

  GET  /control/received  -> every accepted publish so far, as JSON
  POST /control/break     -> every publish from now on fails with 500
  POST /control/restore   -> publishes succeed again
"""

import argparse
import itertools
import json
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

STATE = {"broken": False, "received": [], "ids": itertools.count(1)}
LOCK = threading.Lock()


class Handler(BaseHTTPRequestHandler):
    token = ""

    def log_message(self, *_):
        pass

    def _json(self, status: int, body) -> None:
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        if self.path == "/v1/health":
            return self._json(200, {"healthy": True})
        if self.path == "/control/received":
            with LOCK:
                return self._json(200, list(STATE["received"]))
        self._json(404, {"error": "not found"})

    def do_POST(self):
        if self.path == "/control/break":
            STATE["broken"] = True
            return self._json(200, {})
        if self.path == "/control/restore":
            STATE["broken"] = False
            return self._json(200, {})
        if self.headers.get("Authorization") != f"Bearer {self.token}":
            return self._json(401, {"code": 40101, "error": "unauthorized"})
        if STATE["broken"]:
            return self._json(500, {"code": 50001, "error": "internal server error"})
        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length).decode()
        with LOCK:
            document = {
                "id": f"msg{next(STATE['ids'])}",
                "time": int(time.time()),
                "topic": self.path.lstrip("/"),
                "message": body,
                "title": self.headers.get("Title", ""),
                "priority": int(self.headers.get("Priority", "3")),
            }
            STATE["received"].append(document)
        self._json(200, document)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--token", required=True)
    args = parser.parse_args()
    Handler.token = args.token
    ThreadingHTTPServer(("127.0.0.1", args.port), Handler).serve_forever()


if __name__ == "__main__":
    main()
