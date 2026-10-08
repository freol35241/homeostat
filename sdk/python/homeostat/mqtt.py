"""Shared paho-mqtt plumbing for dialect adapters that bridge MQTT onto the bus.

Each such adapter bridges an external MQTT broker onto the bus (zigbee2mqtt,
OwnTracks, ...).

This module is a helper and not a transport layer (docs/design.md#bus).
Adapters still own their connections, with their own `on_message` logic,
topics and health-event vocabulary. This module covers only the code that
is the same in all of them: endpoint parsing, client construction,
connect-and-subscribe with a SUBACK wait, and the SIGTERM/SIGINT shutdown
wait that runs alongside the zenoh session teardown.
"""

import os
import signal
import threading
import traceback
from pathlib import Path
from urllib.parse import ParseResult, unquote, urlparse

import paho.mqtt.client as mqtt
import tomllib
from paho.mqtt.enums import CallbackAPIVersion

ENV_CREDENTIALS = "HOMEOSTAT_MQTT_CREDENTIALS"


def parse_endpoint(endpoint: str) -> ParseResult:
    """Validate and parse an MQTT endpoint.

    Accepts `mqtt://host[:port][/base/topic]`, or `mqtts://` for TLS with
    default certificate verification.

    Parameters
    ----------
    endpoint : str
        The endpoint URL, as the unit's manifest gives it.

    Returns
    -------
    ParseResult
        The parsed endpoint.

    Raises
    ------
    ValueError
        On any other scheme. Adapters report the message unchanged for a
        misconfigured unit, so its wording matters.
    """
    parsed = urlparse(endpoint)
    if parsed.scheme not in ("mqtt", "mqtts"):
        raise ValueError(f"unsupported endpoint scheme: {endpoint}")
    return parsed


def base_topic(endpoint: ParseResult, default: str) -> str:
    """Return the endpoint's path as a broker topic prefix, or `default` if it has none.

    An installation that has used a non-default prefix for years cannot
    change it, because other consumers use it. The prefix is therefore a
    deployment detail like the host, and is written beside it:
    `mqtt://broker:1883/VP52/zigbee2mqtt`. It is not a secret, so it
    belongs in the repo (docs/design.md#local-only-access).

    Parameters
    ----------
    endpoint : ParseResult
        The endpoint, as `parse_endpoint` returns it.
    default : str
        The prefix to use when the endpoint carries no path.

    Returns
    -------
    str
        The topic prefix, without leading or trailing "/".
    """
    return endpoint.path.strip("/") or default


def credentials(endpoint: ParseResult) -> tuple[str | None, str | None]:
    """Return the username and password for `endpoint`.

    The credentials come from the endpoint (`mqtt://user:pass@host`) when
    present. Otherwise they come from the TOML file named by
    HOMEOSTAT_MQTT_CREDENTIALS. That file lives outside the repo and is
    keyed by broker hostname:

        ["broker.example"]
        username = "homeostat"
        password = "..."

    A broker that needs auth must not force its password into a unit
    manifest. The file works like HOMEOSTAT_ESPHOME_DEVICES
    (docs/design.md#local-only-access). If the variable is unset or the
    file has no entry for this host, the connection is anonymous.

    Parameters
    ----------
    endpoint : ParseResult
        The endpoint, as `parse_endpoint` returns it.

    Returns
    -------
    tuple of (str or None, str or None)
        ``(username, password)``; ``(None, None)`` for anonymous.

    Raises
    ------
    OSError
        If HOMEOSTAT_MQTT_CREDENTIALS names a file that cannot be read.
    tomllib.TOMLDecodeError
        If that file is not valid TOML.
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


def guard(on_message, health=None):
    """Wrap `on_message` to handle two failures that would otherwise stop or hide messages.

    A topic has to become a `str` to be routed, and paho decodes it
    lazily. `msg.topic` is a property, so a topic that is not valid UTF-8
    raises when the adapter first reads it, not when the message arrives.
    Decoding it here turns that into the same typed `drop` the adapters
    already emit for a malformed payload, with the raw bytes attached. A
    broker sending an adapter garbage then shows up as a count on
    home/health/{unit}, instead of only in a container log.

    `health` takes a Session's `health_event`. Without it the message is
    still dropped, but only a traceback is printed.

    Parameters
    ----------
    on_message : callable
        The adapter's paho `on_message(client, userdata, msg)` callback.
    health : callable or None, optional
        A Session's `health_event`, or None.

    Returns
    -------
    callable
        A paho `on_message` callback that runs `on_message` inside the guard.
    """

    def guarded(client, userdata, msg):
        try:
            _ = msg.topic  # force paho's lazy decode inside the try
        except UnicodeDecodeError:
            if health is None:
                traceback.print_exc()
            else:
                # `_topic` holds the undecoded bytes paho keeps. Reading it
                # is the only way to report what arrived. No other code in
                # the project touches it.
                raw = getattr(msg, "_topic", b"")
                health("drop", reason="malformed-topic", topic=repr(raw)[:120])
            return
        try:
            on_message(client, userdata, msg)
        except Exception:
            # paho re-raises callback exceptions out of its network thread.
            # The adapter would stop receiving messages while its
            # liveliness token still says running. Drop the message with a
            # traceback instead. The supervisor captures stderr at
            # home/meta/{unit}/log.
            traceback.print_exc()

    return guarded


def connect(
    endpoint: ParseResult, on_message, topics, *, timeout: float = 30, health=None
) -> mqtt.Client:
    """Build a VERSION2 paho client, connect it to `endpoint` and subscribe `topics`.

    The client calls `on_message` and subscribes `topics` on every
    connect, including reconnects. Blocks until the first SUBACK, and
    raises TimeoutError if the broker does not ack within `timeout`
    seconds.

    `health` is the Session's `health_event`, used to report a message
    dropped for an undecodable topic (see `guard`).

    Starts the network loop in a background thread (`loop_start`). The
    caller then owns the connection and must call `client.loop_stop()` and
    `client.disconnect()` on shutdown.

    Parameters
    ----------
    endpoint : ParseResult
        The broker, as `parse_endpoint` returns it.
    on_message : callable
        The adapter's paho `on_message(client, userdata, msg)`; it runs
        inside `guard`.
    topics : str or list of tuple of (str, int)
        Anything `Client.subscribe` accepts: a topic string or a list of
        (topic, qos) tuples.
    timeout : float, optional
        Seconds to wait for the first SUBACK.
    health : callable or None, optional
        The Session's `health_event`, or None.

    Returns
    -------
    paho.mqtt.client.Client
        The connected client, its network loop running.

    Raises
    ------
    ValueError
        If the endpoint names no host.
    TimeoutError
        If the broker does not ack a subscription within `timeout` seconds.
    ConnectionError
        If, by then, the broker has refused the connection (e.g. bad
        credentials).
    OSError
        If the broker cannot be reached at all.
    """
    subscribed = threading.Event()
    refused: list = []  # a failed CONNACK's reason code, if one arrives
    client = mqtt.Client(CallbackAPIVersion.VERSION2)

    def on_connect(client, userdata, flags, reason_code, properties=None):
        # paho also calls on_connect on a failed CONNACK (bad credentials,
        # broker refusal). Subscribing then would do nothing or raise, and
        # the SUBACK wait below would time out. The error would then wrongly
        # say the broker never answered.
        if reason_code.is_failure:
            refused.append(reason_code)
            return
        client.subscribe(topics)

    client.on_message = guard(on_message, health)
    client.on_connect = on_connect
    client.on_subscribe = lambda *_: subscribed.set()
    username, password = credentials(endpoint)
    if username:
        # Without credentials, a broker that requires auth would show up as
        # a SUBACK timeout.
        client.username_pw_set(username, password)
    if endpoint.scheme == "mqtts":
        client.tls_set()  # system CA store, which must trust the broker's CA
    if not endpoint.hostname:
        raise ValueError(f"endpoint names no host: {endpoint.geturl()}")
    default_port = 8883 if endpoint.scheme == "mqtts" else 1883
    client.connect(endpoint.hostname, endpoint.port or default_port)
    client.loop_start()
    if not subscribed.wait(timeout=timeout):
        if refused:
            raise ConnectionError(f"MQTT broker refused the connection: {refused[-1]}")
        raise TimeoutError(f"no MQTT SUBACK within {int(timeout)}s")
    return client


def wait_for_shutdown() -> None:
    """Block until SIGTERM or SIGINT, the supervisor's stop signal.

    This is the shutdown wait every adapter observes alongside its zenoh
    session teardown.
    """
    stop = threading.Event()
    signal.signal(signal.SIGTERM, lambda *_: stop.set())
    signal.signal(signal.SIGINT, lambda *_: stop.set())
    stop.wait()
