# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "homeostat",
# ]
#
# [tool.uv.sources]
# homeostat = { path = "../sdk/python", editable = true }
# ///
"""go2rtc shim: runs the go2rtc binary as a unit.

See docs/design.md#cameras. The unit contract needs a liveliness token,
which a Go binary cannot declare, so this shim holds it. It renders the
go2rtc config from HOMEOSTAT_CAMERAS (`render_config`), spawns `go2rtc`
from PATH, polls its API until it answers, and then declares ready. When
the child exits the shim exits, so the supervisor's backoff and
process-group sweep apply. The binary comes with the image, not the repo.

go2rtc holds one upstream RTSP session per camera however many browsers
watch, which matters because a Tapo admits only about two concurrent RTSP
clients. Restreaming is a remux only: the image has no ffmpeg, so
anything in go2rtc that transcodes (its frame.jpeg snapshot of an H.264
source) does not work, and nothing here relies on it. Recording, motion
detection and frame storage are out of scope: mature tools do them, and
events come from adapters/onvif.py.

Configuration:

- HOMEOSTAT_CAMERAS: the TOML camera file shared with the onvif adapter.
  Per camera: `host` (bare, or host:port where the port is the ONVIF
  port, not RTSP, which uses 554), `username`, `password`, and optionally
  `stream`, a full RTSP URL that replaces the default
  rtsp://user:pass@host:554/stream1 (Tapo's HD main stream). Each stream
  is named by the camera's entity id, which is how the dashboard
  addresses it.
- HOMEOSTAT_GO2RTC_LISTEN: the API address, default 127.0.0.1:1984.

go2rtc's stdout and stderr go to this unit's, and the supervisor tags
them (docs/design.md#logs-and-the-audit-trail).
"""

import json
import os
import signal
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

import homeostat
import tomllib

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
    # Percent-encode the credentials. A vendor-account password with "/",
    # "?", "#", "@" or a space is normal, and unescaped it truncates or
    # breaks the URL go2rtc parses.
    user = urllib.parse.quote(conf["username"], safe="")
    password = urllib.parse.quote(conf["password"], safe="")
    return f"rtsp://{user}:{password}@{host}:{RTSP_PORT}/stream1"


def render_config(cameras: dict, listen: str) -> dict:
    """Return the go2rtc config as JSON (a YAML subset go2rtc accepts).

    The API listens on localhost and every other listener (go2rtc's own
    RTSP server, WebRTC, SRTP) is off. MSE through the dashboard proxy is
    the only consumer, so browsers never talk to go2rtc, and an
    unauthenticated API with `exec:` sources must not face the LAN. One
    stream per camera.

    The rendered file contains camera credentials, so main writes it to a
    0600 temp file outside the repo and deletes it on exit.
    """
    return {
        "api": {"listen": listen},
        "rtsp": {"listen": ""},
        "webrtc": {"listen": ""},
        "srtp": {"listen": ""},
        "streams": {camera: stream_url(conf) for camera, conf in cameras.items()},
    }


def await_api(listen: str, child: subprocess.Popen, stopping: threading.Event) -> None:
    """Poll /api/streams until go2rtc answers.

    A child that dies first, or never answers, is a startup error, visible
    through the supervisor's backoff. The exception is a stop the
    supervisor asked for.
    """
    deadline = time.monotonic() + READY_TIMEOUT_S
    while time.monotonic() < deadline:
        if child.poll() is not None:
            if stopping.is_set():
                sys.exit(0)
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

    # go2rtc reads this file for as long as it runs, so it is unlinked in
    # the finally block below. A `with` would delete it when the block
    # ends.
    config_file = tempfile.NamedTemporaryFile(  # noqa: SIM115
        mode="w", suffix=".json", prefix="go2rtc-", delete=False
    )
    stopping = threading.Event()
    try:
        json.dump(render_config(cameras, listen), config_file)
        config_file.close()
        try:
            child = subprocess.Popen(["go2rtc", "-config", config_file.name])
        except FileNotFoundError:
            sys.exit("go2rtc not on PATH (it is image-build provisioning, never repo content)")

        def on_signal(signum, frame) -> None:
            stopping.set()
            child.terminate()

        for sig in (signal.SIGTERM, signal.SIGINT):
            signal.signal(sig, on_signal)

        await_api(listen, child, stopping)
        session = homeostat.connect()
        try:
            session.ready()
            code = child.wait()
        finally:
            session.close()
        # go2rtc should not exit on its own, so that is a unit failure
        # whatever its exit code says.
        sys.exit(0 if stopping.is_set() else 1 if code == 0 else code)
    finally:
        os.unlink(config_file.name)


if __name__ == "__main__":
    main()
