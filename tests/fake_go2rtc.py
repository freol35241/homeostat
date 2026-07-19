# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "aiohttp>=3.9,<4",
# ]
# ///
"""A minimal, honest go2rtc, for the go2rtc shim's and the dashboard
camera proxies' integration tests (tests/go2rtc.rs, tests/dashboard.rs;
see docs/design.md, "Cameras (settled 2026-07-19)").

Speaks the slice of go2rtc's surface homeostat touches, and nothing else:

  - `-config <path>`: reads the shim's rendered config (JSON — a YAML
    subset, exactly what the shim writes), takes `api.listen` and the
    stream names from it. `--listen`/`--streams` override for direct
    spawning without a config file (the dashboard test's path).
  - GET /api/streams -> {name: {"producers": [{"url": ...}]}} — what the
    shim polls for readiness and what the tests assert config rendering
    against.
  - GET /api/frame.jpeg?src=<name> -> a tiny JPEG (magic bytes and all);
    404 for an unknown stream.
  - WS /api/ws?src=<name> -> expects the player's {"type":"mse"} request,
    answers the codec line then two binary fMP4-ish frames, then stays
    open; 404 for an unknown stream. Enough for a byte-for-byte relay to
    be asserted through the dashboard.

Test control: POST /control/quit exits abruptly (code 3) — the child
death the shim must translate into its own exit and the supervisor into
a restart.
"""

import argparse
import asyncio
import json
import os
from pathlib import Path

from aiohttp import WSMsgType, web

FAKE_JPEG = b"\xff\xd8\xff\xe0" + b"FAKEJPEG" + b"\xff\xd9"
FAKE_MP4_INIT = b"\x00\x00\x00\x1cftypiso5" + b"FAKEMOOV"
FAKE_MP4_FRAME = b"\x00\x00\x00\x10moof" + b"FAKEFRAME"


def make_app(streams: dict[str, str]) -> web.Application:
    async def api_streams(request: web.Request) -> web.Response:
        return web.json_response(
            {name: {"producers": [{"url": url}]} for name, url in streams.items()}
        )

    async def frame(request: web.Request) -> web.Response:
        if request.query.get("src") not in streams:
            raise web.HTTPNotFound()
        return web.Response(body=FAKE_JPEG, content_type="image/jpeg")

    async def ws(request: web.Request) -> web.WebSocketResponse:
        if request.query.get("src") not in streams:
            raise web.HTTPNotFound()
        socket = web.WebSocketResponse()
        await socket.prepare(request)
        async for message in socket:
            if message.type != WSMsgType.TEXT:
                continue
            if json.loads(message.data).get("type") == "mse":
                await socket.send_str(json.dumps({"type": "mse", "value": "avc1.64001f"}))
                await socket.send_bytes(FAKE_MP4_INIT)
                await socket.send_bytes(FAKE_MP4_FRAME)
        return socket

    async def quit_now(request: web.Request) -> web.Response:
        os._exit(3)

    app = web.Application()
    app.router.add_get("/api/streams", api_streams)
    app.router.add_get("/api/frame.jpeg", frame)
    app.router.add_get("/api/ws", ws)
    app.router.add_post("/control/quit", quit_now)
    return app


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("-config", dest="config")
    parser.add_argument("--listen")
    parser.add_argument("--streams", help="comma-separated stream names (no config file)")
    args = parser.parse_args()

    listen = args.listen
    streams: dict[str, str] = {}
    if args.config:
        config = json.loads(Path(args.config).read_text())
        listen = listen or config.get("api", {}).get("listen")
        streams = dict(config.get("streams", {}))
    if args.streams:
        streams.update({name: f"rtsp://fake/{name}" for name in args.streams.split(",")})
    host, _, port = (listen or "127.0.0.1:1984").rpartition(":")

    web.run_app(make_app(streams), host=host, port=int(port), print=None)


if __name__ == "__main__":
    main()
