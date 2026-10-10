# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "homeostat",
# ]
#
# [tool.uv.sources]
# homeostat = { path = "../sdk/python", editable = true }
# ///
"""ntfy adapter: delivers `notifier` messages through an ntfy server.

See docs/design.md#notifications. ntfy is a small self-hostable push
server with an Android app. It needs no account and no phone number
(Signal and WhatsApp both need a phone number as the house's identity),
it is reached over the LAN or WireGuard like the dashboard, and it speaks
UnifiedPush, the transport a homeostat app would use.

The server is a compose sidecar beside the MQTT broker, not a unit,
because the phones connect to it. ntfy 2.12 and later provisions users,
access rules and tokens from its config on every start
(tests/fixtures/house_reference/ntfy/server.yml). The publisher may only write; each
person reads only their own topic and the group's. That access list
repeats the entity files by hand: rendering it from them would make ntfy
a unit, and the sidecar must not restart when a unit does. A phone
fetches what it missed on reconnect, back to the server's cache-duration,
so it needs a route to the server from anywhere (WireGuard always on).

Binding: an entity file's `id` is the ntfy topic, and the file stem is
the entity name. One entity per addressee: a person's phone in the
pseudo-room `person`, a group topic in `global`.

Configuration: [discovery].endpoint is the server URL (compose-internal,
not a secret). HOMEOSTAT_NTFY_TOKEN is the publisher token, kept out of
the repo; startup fails if it is unset. The live parameter min_interval_s
(owner-editable, default 5 s) is the rate floor for `message` (see `Gate`).

Commands: `message` and `alert`, each a non-empty string, sent at ntfy
priority 3 and 5 (see PRIORITIES). Channels are `shared`, so commands
arrive on home/cmd/{room}/{entity}/{aspect}.

State: `delivered`, the server's time for the last delivered message in
epoch seconds, and `available` (see `main`).

Health events: `drop` with reason malformed-payload, invalid-command,
rate-limited, delivery-failed, queue-full or stale. Discovery is one
static record per entity, with the aspect descriptor.
"""

import json
import os
import queue
import signal
import threading
import time
import urllib.error
import urllib.request
from dataclasses import dataclass

import homeostat
from homeostat import house, keys
from homeostat.params import LiveParams

ENV_TOKEN = "HOMEOSTAT_NTFY_TOKEN"
PARAM_DEFAULTS = {"min_interval_s": 5.0}
HTTP_TIMEOUT_S = 10.0
# Sends queued beyond this drop with `queue-full`, so a dead server sheds
# load instead of queueing forever.
MAX_QUEUE_SIZE = 1000
# A send that waited longer than this drops with `stale`. Fifteen minutes
# covers a server restart and is still timely for a notification.
MAX_QUEUE_AGE_S = 15 * 60

# Commandable aspect -> ntfy priority (1 min .. 5 max). The Android app
# treats 5 as urgent: it overrides Do Not Disturb and plays a continuous
# alarm tone, so the phone itself enforces the message/alert split.
PRIORITIES = {"message": 3, "alert": 5}

ASPECT_DESCRIPTOR = {
    "schema": 1,
    "groups": ["delivery"],
    "fields": {
        "delivered": {"label": "last delivered (epoch s)", "kind": "number", "group": "delivery"},
    },
}


class Params(LiveParams):
    """min_interval_s from home/config/{unit}/*, live."""

    @property
    def min_interval_s(self) -> float:
        return max(0.0, self.get("min_interval_s"))


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    """Refuses every redirect.

    urllib's default handler replays the request (Authorization header
    included) at whatever host the reply names, which would hand the
    publisher token to it.
    """

    def redirect_request(self, *args, **kwargs):
        return None


_opener = urllib.request.build_opener(_NoRedirect)


def check_health(endpoint: str) -> None:
    """Raise unless the server answers `/v1/health` healthy."""
    with _opener.open(f"{endpoint}/v1/health", timeout=HTTP_TIMEOUT_S) as reply:
        body = json.loads(reply.read())
    if not (isinstance(body, dict) and body.get("healthy") is True):
        raise RuntimeError(f"ntfy at {endpoint} is not healthy: {body!r}")


def publish(endpoint: str, token: str, topic: str, text: str, priority: int, title: str) -> dict:
    """Send one POST to the topic and return the server's reply document.

    Raises urllib.error.URLError (HTTPError included) on failure,
    including on a redirect, since the token must reach only the
    configured endpoint.
    """
    request = urllib.request.Request(
        f"{endpoint}/{topic}",
        data=text.encode(),
        method="POST",
        headers={
            "Authorization": f"Bearer {token}",
            "Title": title,
            "Priority": str(priority),
            "Content-Type": "text/plain; charset=utf-8",
        },
    )
    with _opener.open(request, timeout=HTTP_TIMEOUT_S) as reply:
        return json.loads(reply.read())


@dataclass(frozen=True)
class Send:
    """One admitted wish: what to send, at which aspect's priority, titled how."""

    aspect: str
    text: str
    title: str
    cmd_id: str | None


class Gate:
    """Which wishes become sends.

    The envelope must parse, the aspect must be `message` or `alert`, and
    the text a non-blank string; otherwise the wish drops with
    `malformed-payload` or `invalid-command` and does not reach the
    server. The envelope's `actor` becomes the notification's title, so
    the phone says who sent it.

    Rate floor: a `message` within `min_interval_s` of the last send
    attempt to the same entity drops with `rate-limited`. The window is
    per (entity, aspect), so one aspect's traffic does not delay the
    other's. `alert` is exempt: quiet hours and rate limits may withhold
    `message` but not `alert` (docs/design.md#notifications). This floor
    is a safety net. The cooldown that is house policy lives in the
    automation, as a family-editable parameter on the SDK's Cooldown.

    No bus, and the clock is injectable, so tests drive it directly.
    """

    def __init__(self, unit: str, min_interval_s, clock=time.monotonic):
        self.unit = unit
        self.min_interval_s = min_interval_s
        self.clock = clock
        self.last_attempt: dict[tuple[str, str], float] = {}

    def admit(self, entity_name: str, key: str, raw: bytes) -> Send | dict:
        """Return the Send for one cmd sample, or its drop event's fields."""
        aspect = key.split("/", 4)[4]
        try:
            payload = json.loads(raw)
        except ValueError:
            return {"reason": "malformed-payload", "key": key}
        cmd_id = keys.cmd_envelope_id(payload)
        try:
            value = keys.parse_cmd_envelope(payload)
        except ValueError:
            return {"reason": "invalid-command", "key": key, "cmd_id": cmd_id}
        if aspect not in PRIORITIES or not isinstance(value, str) or not value.strip():
            return {
                "reason": "invalid-command",
                "key": key,
                "cmd_id": cmd_id,
                "aspect": aspect,
                "value": value,
            }
        if aspect != "alert":
            now = self.clock()
            floor = self.min_interval_s()
            last = self.last_attempt.get((entity_name, aspect))
            if last is not None and now - last < floor:
                return {"reason": "rate-limited", "key": key, "cmd_id": cmd_id, "min_interval_s": floor}
            self.last_attempt[(entity_name, aspect)] = now
        actor = payload.get("actor")
        # A control character (CR/LF would be header injection) or a
        # non-Latin-1 actor would make the HTTP client raise inside
        # publish(), which would report a bad actor string as a dead
        # server. Use the unit name instead.
        title = (
            actor
            if isinstance(actor, str) and actor and actor.isprintable() and actor.isascii()
            else self.unit
        )
        return Send(aspect, value, title, cmd_id)


def main():
    unit = os.environ[keys.ENV_UNIT]
    config = house.load_adapter(unit)
    endpoint = config.endpoint.rstrip("/")
    token = os.environ.get(ENV_TOKEN)
    if not token:
        raise RuntimeError(f"{ENV_TOKEN} is not set")
    # Not ready until the server answers healthy, so a dead server or a
    # wrong URL shows as supervisor backoff instead of a notifier that
    # drops everything.
    check_health(endpoint)

    session = homeostat.connect()
    params = Params(session, PARAM_DEFAULTS)

    available_lock = threading.Lock()
    available: dict[str, bool] = {}

    # A failed send sets the entity's `available` to false, and the next
    # successful send sets it back. `delivered` keeps its last value
    # through an outage.
    def set_available(entity, value: bool) -> None:
        with available_lock:
            if available.get(entity.name) == value:
                return
            available[entity.name] = value
            session.put_json(keys.state_key(entity.room, entity.name, "available"), value)

    # Sends run one at a time on the sender thread, so a slow server does
    # not stall the bus callback. Items are (entity, aspect, key, text,
    # title, cmd id, enqueued at).
    outbox: queue.Queue = queue.Queue(maxsize=MAX_QUEUE_SIZE)

    def sender():
        while True:
            item = outbox.get()
            if item is None:
                return
            entity, aspect, key, text, title, cmd_id, enqueued_at = item
            if time.monotonic() - enqueued_at > MAX_QUEUE_AGE_S:
                session.health_event("drop", reason="stale", key=key, cmd_id=cmd_id)
                continue
            try:
                reply = publish(endpoint, token, entity.id, text, PRIORITIES[aspect], title)
            except (urllib.error.URLError, OSError, ValueError) as err:
                session.health_event(
                    "drop", reason="delivery-failed", key=key, cmd_id=cmd_id, error=str(err)
                )
                set_available(entity, False)
                continue
            # The server's time for the message: the delivery service's
            # acknowledgment, not a person reading it.
            delivered = reply.get("time") if isinstance(reply, dict) else None
            if isinstance(delivered, (int, float)) and not isinstance(delivered, bool):
                session.put_json(keys.state_key(entity.room, entity.name, "delivered"), delivered)
            set_available(entity, True)

    gate = Gate(unit, lambda: params.min_interval_s)

    def cmd_handler(entity):
        def handler(sample):
            key = str(sample.key_expr)
            send = gate.admit(entity.name, key, sample.payload.to_bytes())
            if not isinstance(send, Send):
                session.health_event("drop", **send)
                return
            item = (entity, send.aspect, key, send.text, send.title, send.cmd_id, time.monotonic())
            try:
                outbox.put_nowait(item)
            except queue.Full:
                session.health_event("drop", reason="queue-full", key=key, cmd_id=send.cmd_id)

        return handler

    subscribers = [
        session.subscribe(expr, cmd_handler(e))
        for e in config.entities
        for expr in keys.command_keyexprs(e)
    ]

    session.put_json(
        keys.discovery_key(unit),
        [
            {
                "id": e.id,
                "configured": True,
                "entity": e.name,
                "suggested": {"capability": "notifier", "features": ["alert"]},
                "aspects": ASPECT_DESCRIPTOR,
            }
            for e in config.entities
        ],
    )

    sender_thread = threading.Thread(target=sender, daemon=True)
    sender_thread.start()

    session.ready()

    stop = threading.Event()
    signal.signal(signal.SIGTERM, lambda *_: stop.set())
    signal.signal(signal.SIGINT, lambda *_: stop.set())
    stop.wait()

    outbox.put(None)
    sender_thread.join(timeout=HTTP_TIMEOUT_S + 1)
    for sub in subscribers:
        sub.undeclare()
    session.close()


if __name__ == "__main__":
    main()
