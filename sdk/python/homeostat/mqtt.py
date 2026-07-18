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

import signal
import threading
from urllib.parse import ParseResult, urlparse

import paho.mqtt.client as mqtt


def parse_endpoint(endpoint: str) -> ParseResult:
    """Validates an `mqtt://host[:port]` endpoint, raising ValueError on
    any other scheme — the message is load-bearing, adapters surface it
    as-is on a misconfigured unit."""
    parsed = urlparse(endpoint)
    if parsed.scheme != "mqtt":
        raise ValueError(f"unsupported endpoint scheme: {endpoint}")
    return parsed


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
    client.on_message = on_message
    client.on_connect = lambda c, *_: c.subscribe(topics)
    client.on_subscribe = lambda *_: subscribed.set()
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
