# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "homeostat",
# ]
#
# [tool.uv.sources]
# homeostat = { path = "../sdk/python", editable = true }
# ///
"""go2rtc shim: the first foreign binary as a unit (see docs/design.md,
"Cameras (settled 2026-07-19)").

The unit contract demands a liveliness token a Go binary cannot declare,
so this thin shim owns it: render the go2rtc config from HOMEOSTAT_CAMERAS
(one stream per camera, named by entity id — the dashboard derives its
proxy targets from that convention), spawn the `go2rtc` binary from PATH
(image-build provisioning, never repo content), poll its API until it
answers, and only then declare ready. Child death means shim exit means
supervisor backoff; the process-group sweep guarantees no orphan.

The config binds go2rtc's API to localhost (HOMEOSTAT_GO2RTC_LISTEN,
default 127.0.0.1:1984 — browsers never speak go2rtc; the dashboard
mediates) and disables its other listeners (its own RTSP re-server,
WebRTC, SRTP): MSE over the dashboard proxy is the only consumer, and an
unauthenticated API with `exec:` sources must not face the LAN. The
rendered file carries camera credentials, so it lives outside the repo in
a 0600 temp file, deleted on exit.

Camera entries: `host` (bare, or host:port — the ONVIF port, which is NOT
the RTSP port; RTSP rides 554), `username`, `password`, and optionally
`stream`, a full RTSP URL overriding the default
rtsp://user:pass@host:554/stream1 (Tapo's HD main stream) for cameras
with a different path. go2rtc's own stdout/stderr ride this unit's — the
supervisor tags them, per the peripheral-logs settlement.
"""

import json
import os
import signal
import subprocess
import sys
import tempfile
import time
import tomllib
import urllib.error
import urllib.request
from pathlib import Path

import homeostat

ENV_CAMERAS = "HOMEOSTAT_CAMERAS"
ENV_LISTEN = "HOMEOSTAT_GO2RTC_LISTEN"
DEFAULT_LISTEN = "127.0.0.1:1984"
RTSP_PORT = 554
READY_TIMEOUT_S = 30
POLL_INTERVAL_S = 0.2


def load_cameras(path: str | None) -> dict:
    if not path:
        return {}
    return tomllib.loads(Path(path).read_text())


def stream_url(conf: dict) -> str:
    if "stream" in conf:
        return conf["stream"]
    host = conf["host"]
    if ":" in host:
        host = host.rpartition(":")[0]
    return f"rtsp://{conf['username']}:{conf['password']}@{host}:{RTSP_PORT}/stream1"


def render_config(cameras: dict, listen: str) -> dict:
    """go2rtc config as JSON (a YAML subset go2rtc accepts): API on
    localhost, every other listener off, one stream per camera."""
    return {
        "api": {"listen": listen},
        "rtsp": {"listen": ""},
        "webrtc": {"listen": ""},
        "srtp": {"listen": ""},
        "streams": {camera: stream_url(conf) for camera, conf in cameras.items()},
    }


def await_api(listen: str, child: subprocess.Popen) -> None:
    """Polls /api/streams until go2rtc answers; a child that dies first, or
    never answers, is a startup error (visible through the supervisor's
    backoff)."""
    deadline = time.monotonic() + READY_TIMEOUT_S
    while time.monotonic() < deadline:
        if child.poll() is not None:
            sys.exit(f"go2rtc exited with {child.returncode} before its API answered")
        try:
            with urllib.request.urlopen(f"http://{listen}/api/streams", timeout=1):
                return
        except (urllib.error.URLError, OSError):
            time.sleep(POLL_INTERVAL_S)
    sys.exit(f"go2rtc API never answered on {listen} within {READY_TIMEOUT_S}s")


def main() -> None:
    cameras = load_cameras(os.environ.get(ENV_CAMERAS))
    listen = os.environ.get(ENV_LISTEN, DEFAULT_LISTEN)

    config_file = tempfile.NamedTemporaryFile(
        mode="w", suffix=".json", prefix="go2rtc-", delete=False
    )
    stopping = False
    try:
        json.dump(render_config(cameras, listen), config_file)
        config_file.close()
        try:
            child = subprocess.Popen(["go2rtc", "-config", config_file.name])
        except FileNotFoundError:
            sys.exit("go2rtc not on PATH (it is image-build provisioning, never repo content)")

        def on_signal(signum, frame) -> None:
            nonlocal stopping
            stopping = True
            child.terminate()

        for sig in (signal.SIGTERM, signal.SIGINT):
            signal.signal(sig, on_signal)

        await_api(listen, child)
        session = homeostat.connect()
        try:
            session.ready()
            code = child.wait()
        finally:
            session.close()
        # A child that dies on its own is a unit failure, whatever its
        # exit code claims — go2rtc has no business exiting.
        sys.exit(0 if stopping else 1 if code == 0 else code)
    finally:
        os.unlink(config_file.name)


if __name__ == "__main__":
    main()
