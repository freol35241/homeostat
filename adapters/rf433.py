# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "homeostat",
#     "paho-mqtt>=2,<3",
# ]
#
# [tool.uv.sources]
# homeostat = { path = "../sdk/python", editable = true }
# ///
"""RF433 adapter: one-way 433 MHz senders behind an OpenMQTTGateway bridge.

A sub-GHz PIR, door contact or smoke detector transmits when something
happens and never sends a "clear". The bridge (OpenMQTTGateway on a Sonoff
RF Bridge, or any gateway publishing the same shape) republishes each
burst on one topic, {base}/SRFBtoMQTT, and only the decimal code in the
payload tells devices apart. The adapter synthesizes the missing `false`
with a hold, following docs/adapters.md#one-way-senders and
docs/design.md#one-way-senders (`Radio`).

Binding: an entity file's `id` is the code, written as a plain decimal
without padding or leading zeros (`code_from`). A PIR binds `presence` and
publishes `occupancy`. A contact or detector binds `binary_sensor` and
publishes the aspect its single feature names (`contact`, `smoke`, ...),
because the radio cannot say what it is (`aspect_for`).

Configuration: the base topic is the endpoint's path, default
`home/OpenMQTTGateway`. Broker credentials are inline in the endpoint or
in HOMEOSTAT_MQTT_CREDENTIALS. Live parameters (owner-editable):
occupancy_hold_s (default 15 s), contact_hold_s (60 s), smoke_hold_s
(600 s), and hold_s (60 s) for any other aspect.

Availability: the bridge's LWT sets `available` for every bound entity.
A receive timer would not work, since silence is a 433 MHz sender's
normal state and says nothing about the gateway.

Discovery: every bound entity with a `heard` flag, then the unbound codes
heard (`Radio.inventory`).

Health events: `misconfigured` (reason no-aspect) once at startup for a
binary_sensor without exactly one feature, and `drop` (malformed-payload)
for a payload with no decodable code.
"""

import json
import os
import threading
import time

import homeostat
from homeostat import house, keys, mqtt
from homeostat.params import LiveParams

DEFAULT_BASE = "home/OpenMQTTGateway"
EVENTS_SUFFIX = "SRFBtoMQTT"
LWT_SUFFIX = "LWT"
ONLINE = "online"

PARAM_DEFAULTS = {
    "occupancy_hold_s": 15.0,
    "contact_hold_s": 60.0,
    "smoke_hold_s": 600.0,
    "hold_s": 60.0,
}

# How often the sweeper looks for expired holds. Not a tuning knob: it only
# bounds how late a `false` can be, and the shortest sensible hold is
# seconds.
TICK_S = 0.25

# Unbound codes come from the wire: anything transmitting nearby lands in
# discovery. Keep the most recent MAX_UNBOUND (evicting the oldest by
# first sighting) and republish at most once per DISCOVERY_COALESCE_S, so
# a busy neighbourhood can neither grow the document without bound nor
# republish it per burst.
MAX_UNBOUND = 200
DISCOVERY_COALESCE_S = 5.0

# Aspect -> the parameter that holds it. The hold is per aspect, not per
# entity: a contact, a PIR and a smoke detector want different holds, but
# every contact wants the same one, so the house tunes a few numbers
# instead of one per device. Anything else falls back to `hold_s`, so an
# unexpected sensor class still decays.
HOLD_PARAM = {
    "occupancy": "occupancy_hold_s",
    "presence": "occupancy_hold_s",
    "contact": "contact_hold_s",
    "smoke": "smoke_hold_s",
}

# Aspects worth surfacing on Now when they go true
# (docs/design.md#aspect-descriptors: notable). A detector firing is a
# deviation; a door opening is not.
NOTABLE = frozenset({"smoke", "gas", "carbon_monoxide", "water_leak"})


class Params(LiveParams):
    """The holds, from home/config/{unit}/*, live."""

    def hold_for(self, aspect: str) -> float:
        return max(0.1, float(self.get(HOLD_PARAM.get(aspect, "hold_s"))))


def aspect_for(entity) -> str | None:
    """Return the aspect a bound entity publishes, or None when the file cannot say.

    `presence` has one in the vocabulary. `binary_sensor` is "a boolean
    under its native name", and the radio cannot say which name, so the
    entity file's single feature names it. Two features would be two
    aspects for a sender that transmits one thing, so that is
    misconfigured too.
    """
    if entity.capability == "presence":
        return entity.features[0] if entity.features else "occupancy"
    return entity.features[0] if len(entity.features) == 1 else None


def code_from(payload: bytes):
    """Return the decimal code in a bridge payload, as a string, or None.

    Two firmware generations: older firmware publishes a bare decimal
    string ("13951014"), current firmware JSON ({"raw": ..., "value":
    13951014, "delay": ...}), read through `value`. Only the bare-decimal
    path has been tested against real hardware (a bridge reporting version
    0.5). The JSON path follows the documented shape and has not been seen
    on a wire, so confirm it if you run current firmware.

    The code is normalized through int, so "13951014", " 13951014 " and
    13951014 are one code: the entity file writes it one way and the wire
    may not.
    """
    text = payload.decode("utf-8", "replace").strip()
    if not text:
        return None
    if text[0] in "{[":
        try:
            document = json.loads(text)
        except ValueError:
            return None
        if not isinstance(document, dict):
            return None
        value = document.get("value")
    else:
        value = text
    try:
        return str(int(value))
    except (TypeError, ValueError):
        return None


class Radio:
    """The bridge-to-bus direction: codes heard, held states, discovery.

    A bound code drives its entity's aspect true and holds it for
    `hold_for(aspect)` seconds from the latest burst; `expire` sends it
    false once the hold has passed. One lock guards the deadlines and the
    publishes that follow from them. A burst and an expiry for the same
    entity may run on different threads in the same tick, and a `false`
    published after the lock is released could overtake the burst's
    `true`, leaving the entity `false` for a whole hold while its deadline
    stands. A code heard for the first time is reported through
    `on_new_code`, called outside the lock. `session` is anything with
    `put_json` and `health_event`; the clock is injectable.

    The hold is this adapter's job and not consumer-side debouncing,
    because every consumer would reimplement it differently, and the
    recorder could not reconstruct what was true when.
    """

    def __init__(self, session, base: str, entities, hold_for, on_new_code, clock=time.monotonic):
        self.session = session
        self.base = base
        self.entities = list(entities)
        self.hold_for = hold_for
        self.on_new_code = on_new_code
        self.clock = clock
        self.by_code = {e.id: e for e in self.entities}
        self.by_name = {e.name: e for e in self.entities}
        self.aspects = {e.name: aspect_for(e) for e in self.entities}
        self.lock = threading.Lock()
        self.sighted: set[str] = set()  # bound codes heard
        self.unbound: dict[str, None] = {}  # codes bound to nothing, oldest first
        self.deadline: dict[str, float] = {}  # entity name -> expiry on the clock

    def publish(self, entity, value: bool) -> None:
        """Publish an entity's one aspect."""
        key = keys.state_key(entity.room, entity.name, self.aspects[entity.name])
        self.session.put_json(key, value)

    def descriptor(self, entity) -> dict:
        """Return the aspect descriptor of a bound entity: its one boolean."""
        aspect = self.aspects[entity.name]
        field = {"label": aspect.replace("_", " "), "kind": "boolean", "group": "readings"}
        if aspect in NOTABLE:
            field["notable"] = True
        return {"schema": 1, "groups": ["readings"], "fields": {aspect: field}}

    def inventory(self) -> list[dict]:
        """Return the discovery document: bound entities, then codes bound to nothing.

        Bound entities are listed whether or not they have transmitted,
        because the dashboard renders from their descriptors, and a door
        nobody opened today still has a card. `heard` says whether this
        adapter has heard the code. A smoke detector's field is `notable`,
        so a detector firing shows on Now as a deviation.

        Unbound codes are normal on 433 MHz: neighbours' remotes, a car
        key, the doorbell. Discovery is how a device gets identified: press
        it, watch the code appear, write the entity file. Unbound codes
        therefore get no health event. Their suggestion is generic,
        because the radio says nothing about what transmitted. Only the
        MAX_UNBOUND most recently first-heard are kept (see note()).
        """
        with self.lock:
            records = []
            for entity in self.entities:
                record = {
                    "id": entity.id,
                    "configured": True,
                    "entity": entity.name,
                    "heard": entity.id in self.sighted,
                    "suggested": {
                        "capability": entity.capability,
                        "features": list(entity.features),
                    },
                }
                if self.aspects[entity.name] is not None:
                    record["aspects"] = self.descriptor(entity)
                records.append(record)
            for code in sorted(self.unbound):
                records.append({
                    "id": code,
                    "configured": False,
                    "entity": None,
                    "heard": True,
                    "suggested": {"capability": "binary_sensor", "features": []},
                })
            return records

    def note(self, code: str) -> bool:
        """Record a code as heard; True the first time, when discovery has news."""
        with self.lock:
            if code in self.by_code:
                if code in self.sighted:
                    return False
                self.sighted.add(code)
            else:
                if code in self.unbound:
                    return False
                self.unbound[code] = None
                if len(self.unbound) > MAX_UNBOUND:
                    del self.unbound[next(iter(self.unbound))]
            return True

    def start(self) -> None:
        """Publish false for every bound entity at startup.

        A held `true` is this adapter's own construct, not a device
        reading, so after a restart "nothing has asserted within the hold"
        is the correct state. It also means a crash-looping adapter cannot
        leave a motion sensor stuck on, since every restart clears it. Only
        an adapter whose breaker is open leaves a `true` standing, and its
        unit health shows that. Restoring the last published value would be
        wrong: it would bring back an assertion whose hold has expired.

        An entity whose file names no aspect is reported once here, not
        per burst.
        """
        for entity in self.entities:
            if self.aspects[entity.name] is None:
                self.session.health_event(
                    "misconfigured", reason="no-aspect", entity=entity.name,
                    hint="a binary_sensor needs exactly one feature naming its aspect",
                )
                continue
            self.publish(entity, False)

    def on_message(self, topic: str, raw: bytes) -> None:
        """Translate one bridge message: its LWT, or a code heard."""
        if topic == f"{self.base}/{LWT_SUFFIX}":
            online = raw.decode("utf-8", "replace").strip().lower() == ONLINE
            for entity in self.entities:
                self.session.put_json(keys.state_key(entity.room, entity.name, "available"), online)
            return

        code = code_from(raw)
        if code is None:
            self.session.health_event("drop", reason="malformed-payload", topic=topic)
            return

        if self.note(code):
            self.on_new_code()
        entity = self.by_code.get(code)
        if entity is None:
            return  # an unbound code; discovery carries it, the log does not
        aspect = self.aspects[entity.name]
        if aspect is None:
            return  # misconfigured; reported once at startup, not per burst

        with self.lock:
            fresh = entity.name not in self.deadline
            self.deadline[entity.name] = self.clock() + self.hold_for(aspect)
            if fresh:
                # Transitions only. 433 MHz senders repeat each burst
                # several times, so a repeat inside the hold only extends
                # the deadline.
                self.publish(entity, True)

    def expire(self) -> None:
        """Send false for every hold that has passed."""
        now = self.clock()
        with self.lock:
            expired = [name for name, when in self.deadline.items() if when <= now]
            for name in expired:
                del self.deadline[name]
                self.publish(self.by_name[name], False)


def main():
    unit = os.environ[keys.ENV_UNIT]
    config = house.load_adapter(unit)
    endpoint = mqtt.parse_endpoint(config.endpoint)
    base = mqtt.base_topic(endpoint, DEFAULT_BASE)

    session = homeostat.connect()
    params = Params(session, PARAM_DEFAULTS)

    # New codes are published together: one timer pending at a time.
    timer_lock = threading.Lock()
    discovery_timer: list[threading.Timer | None] = [None]

    def publish_discovery() -> None:
        with timer_lock:
            discovery_timer[0] = None
        session.put_json(keys.discovery_key(unit), radio.inventory())

    def schedule_discovery() -> None:
        with timer_lock:
            if discovery_timer[0] is None:
                discovery_timer[0] = threading.Timer(DISCOVERY_COALESCE_S, publish_discovery)
                discovery_timer[0].daemon = True
                discovery_timer[0].start()

    radio = Radio(session, base, config.entities, params.hold_for, schedule_discovery)
    stop = threading.Event()

    def sweeper():
        while not stop.wait(TICK_S):
            radio.expire()

    client = mqtt.connect(
        endpoint,
        lambda _client, _userdata, msg: radio.on_message(msg.topic, msg.payload),
        [(f"{base}/{EVENTS_SUFFIX}", 0), (f"{base}/{LWT_SUFFIX}", 0)],
    )

    radio.start()
    session.put_json(keys.discovery_key(unit), radio.inventory())

    threading.Thread(target=sweeper, daemon=True, name="rf433-sweeper").start()

    session.ready()

    mqtt.wait_for_shutdown()

    stop.set()
    # Stop the MQTT loop first, or an in-flight on_message could put on
    # a closed zenoh session.
    client.loop_stop()
    client.disconnect()
    with timer_lock:
        if discovery_timer[0] is not None:
            discovery_timer[0].cancel()
    session.close()


if __name__ == "__main__":
    main()
