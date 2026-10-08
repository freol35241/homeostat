"""Shared paho-mqtt plumbing for dialect adapters that bridge MQTT onto the bus.

Each such adapter bridges an external MQTT broker onto the bus (zigbee2mqtt,
OwnTracks, ...).

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
import traceback
from pathlib import Path
from urllib.parse import ParseResult, unquote, urlparse

import paho.mqtt.client as mqtt
import tomllib

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
        On any other scheme. The message is load-bearing: adapters surface
        it as-is on a misconfigured unit.
    """
    parsed = urlparse(endpoint)
    if parsed.scheme not in ("mqtt", "mqtts"):
        raise ValueError(f"unsupported endpoint scheme: {endpoint}")
    return parsed


def base_topic(endpoint: ParseResult, default: str) -> str:
    """Return the endpoint's path as a broker topic prefix, or `default` if it has none.

    An estate that has run a non-default prefix for years cannot move it —
    other consumers address it — so the prefix is a deployment fact of the
    same kind as the host, and lives beside it:
    `mqtt://broker:1883/VP52/zigbee2mqtt`. Not a secret, so the repo is
    the right place for it (docs/design.md, the boundary test).

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

    Inline `mqtt://user:pass@host` when present, otherwise the
    HOMEOSTAT_MQTT_CREDENTIALS TOML — a file OUTSIDE the repo, keyed by
    broker hostname:

        ["broker.example"]
        username = "homeostat"
        password = "..."

    A broker that needs auth must not force its password into a unit
    manifest; the file mirrors HOMEOSTAT_ESPHOME_DEVICES (docs/design.md,
    the boundary test). Unset env var or no entry for this host: anonymous.

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
    """Wrap `on_message` in the two failures that would otherwise leave it deaf or silent.

    A topic has to become a `str` to be routed, and paho decodes it
    lazily — `msg.topic` is a property, so a topic that is not valid UTF-8
    raises at the adapter's first *access* rather than at receive. Forcing
    the decode here turns that into the same typed `drop` the adapters
    already emit for a malformed payload, with the raw bytes attached, so
    a broker feeding an adapter garbage is countable on home/health/{unit}
    instead of archaeology in a container log.

    `health` takes a Session's `health_event`. Without one the drop still
    happens but only as a trace, which is what an adapter that never
    passes it gets today.

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
            _ = msg.topic  # force paho's lazy decode inside the guard
        except UnicodeDecodeError:
            if health is None:
                traceback.print_exc()
            else:
                # `_topic` is the undecoded bytes paho keeps; reading it is
                # the only way to report what actually arrived, and this is
                # the one place in the estate that touches it.
                raw = getattr(msg, "_topic", b"")
                health("drop", reason="malformed-topic", topic=repr(raw)[:120])
            return
        try:
            on_message(client, userdata, msg)
        except Exception:
            # paho re-raises callback exceptions out of its network thread,
            # which would leave the adapter deaf while its liveliness token
            # still says running. Drop the message with a trace instead;
            # the supervisor captures stderr at home/meta/{unit}/log.
            traceback.print_exc()

    return guarded


def connect(
    endpoint: ParseResult, on_message, topics, *, timeout: float = 30, health=None
) -> mqtt.Client:
    """Build a VERSION2 paho client, connect it to `endpoint` and subscribe `topics`.

    The client is wired to `on_message` and (re)subscribes `topics` on
    every connect, including reconnects. Blocks until the first SUBACK,
    raising TimeoutError if the broker never acks within `timeout` seconds.

    `health` is the Session's `health_event`, used to report a message
    dropped for an undecodable topic (see `guard`).

    Starts the network loop in a background thread (`loop_start`); the
    caller owns the connection from here and is responsible for
    `client.loop_stop()` / `client.disconnect()` on shutdown.

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
    TimeoutError
        If the broker never acks a subscription within `timeout` seconds.
    ConnectionError
        If, by then, the broker has refused the connection (e.g. bad
        credentials).
    OSError
        If the broker cannot be reached at all.
    """
    subscribed = threading.Event()
    refused: list = []  # a failed CONNACK's reason code, if one arrives
    client = mqtt.Client(mqtt.CallbackAPIVersion.VERSION2)

    def on_connect(client, userdata, flags, reason_code, properties=None):
        # paho calls on_connect on a failed CONNACK too (bad credentials,
        # broker refusal); subscribing then would either no-op or raise,
        # and the SUBACK wait below would time out — misdiagnosing the
        # refusal as "the broker never answered".
        if reason_code.is_failure:
            refused.append(reason_code)
            return
        client.subscribe(topics)

    client.on_message = guard(on_message, health)
    client.on_connect = on_connect
    client.on_subscribe = lambda *_: subscribed.set()
    username, password = credentials(endpoint)
    if username:
        # Silently dropping credentials misdiagnoses an auth-requiring
        # broker as a SUBACK timeout.
        client.username_pw_set(username, password)
    if endpoint.scheme == "mqtts":
        client.tls_set()  # system CA store; the broker's own if trusted there
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
