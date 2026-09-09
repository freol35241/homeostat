# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "homeostat",
# ]
#
# [tool.uv.sources]
# homeostat = { path = "../sdk/python", editable = true }
# ///
"""ntfy notifier adapter (docs/design.md, "Notifications (settled
2026-09-09, #31)" and "The ntfy adapter").

The first delivery dialect for the `notifier` capability. An ntfy server
(self-hosted, a compose sidecar beside the MQTT broker — the phones
connect to IT, so it is not a unit) takes one HTTP POST per message on
`{endpoint}/{topic}`; the family's phones subscribe to topics in the ntfy
app. The entity file's `id` is the topic; the file stem is the entity
name; one entity per addressee — a person's phone in the pseudo-room
`person`, a group topic in `global`. The manifest's `[discovery].endpoint`
is the server URL (compose-internal, not a secret). The publisher token is
HOMEOSTAT_NTFY_TOKEN in the environment, never in the repo; unset is a
startup error.

Startup GETs `{endpoint}/v1/health` and refuses to declare ready until the
server answers healthy — a dead server or a wrong URL is the supervisor's
backoff, not a notifier that silently drops everything (the SMTP relay
that returned 250 OK for two weeks, #31).

Commands (the two commandable aspects of the vocabulary; every channel is
`shared`, so they arrive on home/cmd/{room}/{entity}/{aspect}):

- `message`: a non-empty string -> POST at ntfy priority 3 (default).
- `alert`: a non-empty string -> POST at ntfy priority 5 (max), which the
  Android app treats as urgent: it overrides Do Not Disturb and plays a
  continuous alarm tone. The severity split of the vocabulary is
  therefore enforced by the phone itself.

The envelope's `actor` (the sending unit) becomes the notification's
Title, so the phone says who spoke. Anything else — a wrong type, an
empty string, an unknown aspect, a malformed or envelope-less payload —
DROPS with "invalid-command" (or "malformed-payload") and never reaches
the server.

Rate floor: `min_interval_s` (parameter, owner-editable, default 5 s) per
entity; a message inside the window since the last SEND ATTEMPT drops
with reason "rate-limited". This is defense in depth (the ivt490 bounds
argument): the cooldown that is house policy lives in the automation as
a family-editable parameter on the SDK's Cooldown.

Delivery: sends are serialised on one worker thread so a slow server
never stalls the bus callback. The server's JSON reply carries the
message's server time; it publishes as `delivered` (epoch seconds, the
`fixed_at` shape) — the delivery service's acknowledgment, never a
human's. A non-2xx reply or a connection error drops the message with
reason "delivery-failed" (the error text in the event) and flips
home/state/{room}/{entity}/available to false; the next successful send
flips it back. Stale, never false: `delivered` stands across an outage.

Health events: drop/malformed-payload, drop/invalid-command,
drop/rate-limited, drop/delivery-failed. Discovery is the static
one-record-per-entity document with the aspect descriptor.
"""

import json
import os
import queue
import signal
import threading
import time
import urllib.error
import urllib.request

import homeostat
from homeostat import house, keys
from homeostat.params import LiveParams

ENV_TOKEN = "HOMEOSTAT_NTFY_TOKEN"
PARAM_DEFAULTS = {"min_interval_s": 5.0}
HTTP_TIMEOUT_S = 10.0

# Commandable aspect -> ntfy priority (1 min .. 5 max).
PRIORITIES = {"message": 3, "alert": 5}

ASPECT_DESCRIPTOR = {
    "schema": 1,
    "groups": ["delivery"],
    "fields": {
        "delivered": {"label": "last delivered (epoch s)", "kind": "number", "group": "delivery"},
    },
}


class Params(LiveParams):
    @property
    def min_interval_s(self) -> float:
        return max(0.0, self.get("min_interval_s"))


def check_health(endpoint: str) -> None:
    """Raises unless the server answers `/v1/health` healthy."""
    with urllib.request.urlopen(f"{endpoint}/v1/health", timeout=HTTP_TIMEOUT_S) as reply:
        body = json.loads(reply.read())
    if not (isinstance(body, dict) and body.get("healthy") is True):
        raise RuntimeError(f"ntfy at {endpoint} is not healthy: {body!r}")


def publish(endpoint: str, token: str, topic: str, text: str, priority: int, title: str) -> dict:
    """One POST to the topic; returns the server's reply document. Raises
    urllib.error.URLError (HTTPError included) on failure."""
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
    with urllib.request.urlopen(request, timeout=HTTP_TIMEOUT_S) as reply:
        return json.loads(reply.read())


def main():
    unit = os.environ[keys.ENV_UNIT]
    config = house.load_adapter(unit)
    endpoint = config.endpoint.rstrip("/")
    token = os.environ.get(ENV_TOKEN)
    if not token:
        raise RuntimeError(f"{ENV_TOKEN} is not set")
    check_health(endpoint)

    session = homeostat.connect()
    params = Params(session, PARAM_DEFAULTS)

    available_lock = threading.Lock()
    available: dict[str, bool] = {}

    def set_available(entity, value: bool) -> None:
        with available_lock:
            if available.get(entity.name) == value:
                return
            available[entity.name] = value
            session.put_json(keys.state_key(entity.room, entity.name, "available"), value)

    # Sends serialised on one thread: (entity, aspect, key, text, title).
    outbox: queue.Queue = queue.Queue()
    last_attempt: dict[str, float] = {}

    def sender():
        while True:
            item = outbox.get()
            if item is None:
                return
            entity, aspect, key, text, title = item
            try:
                reply = publish(endpoint, token, entity.id, text, PRIORITIES[aspect], title)
            except (urllib.error.URLError, OSError, ValueError) as err:
                session.health_event("drop", reason="delivery-failed", key=key, error=str(err))
                set_available(entity, False)
                continue
            delivered = reply.get("time") if isinstance(reply, dict) else None
            if isinstance(delivered, (int, float)) and not isinstance(delivered, bool):
                session.put_json(keys.state_key(entity.room, entity.name, "delivered"), delivered)
            set_available(entity, True)

    def cmd_handler(entity):
        def handler(sample):
            key = str(sample.key_expr)
            aspect = key.split("/", 4)[4]
            try:
                payload = json.loads(sample.payload.to_bytes())
            except ValueError:
                session.health_event("drop", reason="malformed-payload", key=key)
                return
            try:
                value = keys.parse_cmd_envelope(payload)
            except ValueError:
                session.health_event("drop", reason="invalid-command", key=key)
                return
            if aspect not in PRIORITIES or not isinstance(value, str) or not value.strip():
                session.health_event(
                    "drop", reason="invalid-command", key=key, aspect=aspect, value=value
                )
                return
            now = time.monotonic()
            last = last_attempt.get(entity.name)
            if last is not None and now - last < params.min_interval_s:
                session.health_event(
                    "drop", reason="rate-limited", key=key, min_interval_s=params.min_interval_s
                )
                return
            last_attempt[entity.name] = now
            actor = payload.get("actor")
            title = actor if isinstance(actor, str) and actor else unit
            outbox.put((entity, aspect, key, value, title))

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
