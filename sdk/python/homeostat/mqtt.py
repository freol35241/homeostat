"""Shared paho-mqtt plumbing for dialect adapters that bridge an external
MQTT broker onto the bus (zigbee2mqtt, OwnTracks, ...).

A helper, not a transport layer (docs/design.md, IVT490 heat-pump adapter):
adapters still own their connections — their own `on_message` logic, their
own topics, their own health-event vocabulary. This module only covers the
plumbing that is identical across all of them: endpoint parsing, client
construction, connect-and-subscribe with a SUBACK wait, and the
SIGTERM/SIGINT shutdown wait that runs alongside the zenoh session
teardown.
"""

import os
import signal
import threading
import tomllib
import traceback
from pathlib import Path
from urllib.parse import ParseResult, unquote, urlparse

import paho.mqtt.client as mqtt

ENV_CREDENTIALS = "HOMEOSTAT_MQTT_CREDENTIALS"


def parse_endpoint(endpoint: str) -> ParseResult:
    """Validates an `mqtt://host[:port][/base/topic]` endpoint, raising
    ValueError on any other scheme — the message is load-bearing, adapters
    surface it as-is on a misconfigured unit."""
    parsed = urlparse(endpoint)
    if parsed.scheme != "mqtt":
        raise ValueError(f"unsupported endpoint scheme: {endpoint}")
    return parsed


def base_topic(endpoint: ParseResult, default: str) -> str:
    """The endpoint's path as a broker topic prefix, or `default` when it
    carries none. An estate that has run a non-default prefix for years
    cannot move it — other consumers address it — so the prefix is a
    deployment fact of the same kind as the host, and lives beside it:
    `mqtt://broker:1883/VP52/zigbee2mqtt`. Not a secret, so the repo is
    the right place for it (docs/design.md, the boundary test)."""
    return endpoint.path.strip("/") or default


def credentials(endpoint: ParseResult) -> tuple[str | None, str | None]:
    """Username/password for `endpoint`: inline `mqtt://user:pass@host`
    when present, otherwise the HOMEOSTAT_MQTT_CREDENTIALS TOML — a file
    OUTSIDE the repo, keyed by broker hostname:

        ["broker.example"]
        username = "homeostat"
        password = "..."

    A broker that needs auth must not force its password into a unit
    manifest; the file mirrors HOMEOSTAT_ESPHOME_DEVICES (docs/design.md,
    the boundary test). Unset env var or no entry for this host: anonymous.
    """
    if endpoint.username:
        return unquote(endpoint.username), (
            unquote(endpoint.password) if endpoint.password else None
        )
    path = os.environ.get(ENV_CREDENTIALS)
    if not path:
        return None, None
    entry = tomllib.loads(Path(path).read_text()).get(endpoint.hostname or "") or {}
    return entry.get("username"), entry.get("password")


def connect(endpoint: ParseResult, on_message, topics, *, timeout: float = 30) -> mqtt.Client:
    """Builds a VERSION2 paho client wired to `on_message`, connects to
    `endpoint`, and (re)subscribes `topics` — anything `Client.subscribe`
    accepts, a topic string or a list of (topic, qos) tuples — on every
    connect, including reconnects. Blocks until the first SUBACK, raising
    TimeoutError if the broker never acks within `timeout` seconds.

    Starts the network loop in a background thread (`loop_start`); the
    caller owns the connection from here and is responsible for
    `client.loop_stop()` / `client.disconnect()` on shutdown.
    """
    subscribed = threading.Event()
    client = mqtt.Client(mqtt.CallbackAPIVersion.VERSION2)

    def guarded(client, userdata, msg):
        try:
            on_message(client, userdata, msg)
        except Exception:
            # paho re-raises callback exceptions out of its network thread,
            # which would leave the adapter deaf while its liveliness token
            # still says running. Drop the message with a trace instead;
            # the supervisor captures stderr at home/meta/{unit}/log.
            traceback.print_exc()

    client.on_message = guarded
    client.on_connect = lambda c, *_: c.subscribe(topics)
    client.on_subscribe = lambda *_: subscribed.set()
    username, password = credentials(endpoint)
    if username:
        # Silently dropping credentials misdiagnoses an auth-requiring
        # broker as a SUBACK timeout.
        client.username_pw_set(username, password)
    client.connect(endpoint.hostname, endpoint.port or 1883)
    client.loop_start()
    if not subscribed.wait(timeout=timeout):
        raise TimeoutError(f"no MQTT SUBACK within {int(timeout)}s")
    return client


def wait_for_shutdown() -> None:
    """Blocks until SIGTERM or SIGINT — the supervisor's stop signal —
    the shutdown wait every adapter observes alongside its zenoh session
    teardown."""
    stop = threading.Event()
    signal.signal(signal.SIGTERM, lambda *_: stop.set())
    signal.signal(signal.SIGINT, lambda *_: stop.set())
    stop.wait()
