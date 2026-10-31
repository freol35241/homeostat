# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "homeostat",
# ]
#
# [tool.uv.sources]
# homeostat = { path = "../sdk/python", editable = true }
# ///
"""Persons: each person computed from the trackers that person carries.

See docs/design.md#map-and-people. A person is an entity file under this
unit's entities dir. Its `[sources]` list the person's trackers: `home`
(a geofence) or `sighted` (seen on the home network) for presence, `lat`
for a position fix. The plan refuses any other source. The house writes
which trackers are whose; the rule is fixed (`Persons.evaluate`).

`presence`:

1. home if any usable tracker reports `home` or `sighted` as true;
2. away if none does and either a usable tracker reports `home` as
   false, or the person lists no `home` source and every usable
   `sighted` tracker reports false;
3. otherwise unchanged. Losing sight of someone is not evidence that
   they left.

A tracker is usable unless it reports `available = false`. The second
case under rule 2 lets a person tracked only by WiFi be away; the
adapter's own delay already stands behind that `false`.

Position: `lat`, `lon`, `accuracy` and `fixed_at` come from the newest
fix whose accuracy is within max_accuracy_m. A fix without `fixed_at` is
dated when it arrived. When no fix qualifies, the last values stand.
`battery` belongs to the device and stays on the tracker.

`available` is false only when every tracker reports `available = false`.

Each value publishes on change. Which sources took part is reported with
`ctx.source_used`, and why presence is what it is with `ctx.decision`,
one rule per person.

Manifest: subscribe `home/state/person/*/*`, where trackers live, and
publish `home/state/{room}/{entity}/*`. Configuration: the live
parameter max_accuracy_m (owner-editable).
"""

import threading
import time
from collections.abc import Callable
from dataclasses import dataclass, field
from typing import Any

from homeostat import automation, house

PRESENCE_ASPECTS = ("home", "sighted")
# Tracker aspects this unit reads: presence evidence, a fix and liveness.
READ_ASPECTS = frozenset({"home", "sighted", "lat", "lon", "accuracy", "fixed_at", "available"})


def number(value: Any) -> float | None:
    """Return a JSON number as a float, or None for anything else."""
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    return float(value)


@dataclass
class Outcome:
    """One evaluation of one person.

    Attributes
    ----------
    values : dict of str to Any
        The aspects the person carries now. An aspect left out keeps its
        last value.
    used : dict of (str, str) to bool
        (computed aspect, source name) to whether the source took part.
    decision : str
        Why presence is what it is, in words.
    """

    values: dict[str, Any] = field(default_factory=dict)
    used: dict[tuple[str, str], bool] = field(default_factory=dict)
    decision: str = ""


class Persons:
    """The latest tracker readings, and the rule that turns them into persons.

    Parameters
    ----------
    persons : list of house.Entity
        The person entities this unit binds, with their `[sources]`.
    clock : Callable[[], float], optional
        Wall-clock seconds, which date a fix that carries no `fixed_at`.
    """

    def __init__(self, persons: list[house.Entity], clock: Callable[[], float] = time.time):
        self.persons = persons
        self.clock = clock
        self.trackers = {src.entity for p in persons for src in p.sources.values()}
        # tracker -> aspect -> last value
        self.readings: dict[str, dict[str, Any]] = {}
        # tracker -> when its last `lat` was taken, by this unit's clock
        self.received: dict[str, float] = {}

    def seen(self, tracker: str, aspect: str, value: Any, age_s: float = 0.0) -> list[house.Entity]:
        """Record one tracker reading and return the persons it concerns."""
        if tracker not in self.trackers or aspect not in READ_ASPECTS:
            return []
        self.readings.setdefault(tracker, {})[aspect] = value
        if aspect == "lat":
            self.received[tracker] = self.clock() - age_s
        return [p for p in self.persons if any(s.entity == tracker for s in p.sources.values())]

    def usable(self, tracker: str) -> bool:
        """Whether a tracker's presence evidence counts: it is not reported unavailable."""
        return self.readings.get(tracker, {}).get("available") is not False

    def evaluate(self, person: house.Entity, max_accuracy_m: float) -> Outcome:
        """Apply the persons rule to one person."""
        out = Outcome()
        self._presence(person, out)
        self._position(person, max_accuracy_m, out)
        trackers = {src.entity for src in person.sources.values()}
        if trackers:
            out.values["available"] = not all(
                self.readings.get(t, {}).get("available") is False for t in trackers
            )
        return out

    def _presence(self, person: house.Entity, out: Outcome) -> None:
        says: dict[str, bool] = {}  # source name -> the usable tracker's boolean
        geofenced = False
        for name, src in person.sources.items():
            if src.aspect not in PRESENCE_ASPECTS:
                continue
            geofenced = geofenced or src.aspect == "home"
            value = self.readings.get(src.entity, {}).get(src.aspect)
            took_part = self.usable(src.entity) and isinstance(value, bool)
            out.used[("presence", name)] = took_part
            if took_part:
                says[name] = value
        sources = person.sources
        home = sorted(n for n, v in says.items() if v)
        left = sorted(n for n, v in says.items() if not v and sources[n].aspect == "home")
        unseen = sorted(n for n, v in says.items() if not v and sources[n].aspect == "sighted")
        # Every usable `sighted` source, for the fallback: all of them must say false.
        sighted = [
            n for n, src in sources.items()
            if src.aspect == "sighted" and self.usable(src.entity)
        ]
        if home:
            out.values["presence"] = True
            out.decision = f"home: {', '.join(home)} says so"
        elif left:
            out.values["presence"] = False
            out.decision = f"away: {', '.join(left)} left the geofence"
        elif not geofenced and sighted and sorted(sighted) == unseen:
            out.values["presence"] = False
            out.decision = f"away: no geofence, and {', '.join(unseen)} has not seen the device"
        else:
            out.decision = "unchanged: no usable tracker says home or left"

    def _position(self, person: house.Entity, max_accuracy_m: float, out: Outcome) -> None:
        best: tuple[float, str, dict] | None = None
        for name, src in person.sources.items():
            if src.aspect != "lat":
                continue
            reading = self.readings.get(src.entity, {})
            lat, lon = number(reading.get("lat")), number(reading.get("lon"))
            accuracy = number(reading.get("accuracy"))
            out.used[("lat", name)] = False
            if lat is None or lon is None:
                continue
            if accuracy is not None and accuracy > max_accuracy_m:
                continue
            # The tracker's own values, so the person carries what it published.
            fix = {"lat": reading["lat"], "lon": reading["lon"]}
            if accuracy is not None:
                fix["accuracy"] = reading["accuracy"]
            fixed_at = number(reading.get("fixed_at"))
            if fixed_at is None:
                fixed_at = self.received.get(src.entity, self.clock())
                fix["fixed_at"] = fixed_at
            else:
                fix["fixed_at"] = reading["fixed_at"]
            if best is None or fixed_at > best[0]:
                best = (fixed_at, name, fix)
        if best is not None:
            out.used[("lat", best[1])] = True
            out.values.update(best[2])


def main():
    ctx = automation.context()
    persons = [
        e for e in house.load_house().entities
        if e.owner == ctx.unit and e.capability == "person"
    ]
    rule = Persons(persons)
    lock = threading.Lock()
    published: dict[tuple[str, str], Any] = {}

    def on_tracker(key, value, age_s):
        _, _, _room, tracker, aspect = key.split("/")
        with lock:
            for person in rule.seen(tracker, aspect, value, age_s):
                out = rule.evaluate(person, ctx.params.max_accuracy_m)
                for aspect_name, v in out.values.items():
                    if published.get((person.name, aspect_name)) != v:
                        ctx.publish("person", v, room=person.room, entity=person.name, aspect=aspect_name)
                        published[(person.name, aspect_name)] = v
                for (computed, source), used in out.used.items():
                    ctx.source_used(person.name, computed, source, used)
                ctx.decision(person.name, out.decision)

    ctx.subscribe("trackers", on_tracker)
    ctx.ready()
    ctx.run()


if __name__ == "__main__":
    main()
