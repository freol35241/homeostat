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
adapter's own delay already stands behind that `false`. `presence_valid`
is false while presence is held by rule 3, so the dashboard shows it as
stale.

Position: `lat`, `lon`, `accuracy` and `fixed_at` come from the newest
fix whose accuracy is within max_accuracy_m. A fix counts once its
`fixed_at` arrives, which a tracker publishes after the fix's other
aspects (`Persons.seen`). When no fix qualifies, the last values stand. `battery` belongs to the device and stays on the
tracker.

`available` is true when any tracker reports it true and false when
every tracker reports it false. Without a report it is not published.

Each value publishes on change. The sources that decided each value are
reported with `ctx.source_used`, and why presence is what it is with
`ctx.decision`, one rule per person.

Manifest: subscribe `home/state/person/*/*`, where trackers live, and
publish `home/state/{room}/{entity}/*`. Configuration: the live
parameter max_accuracy_m (owner-editable).
"""

import threading
from dataclasses import dataclass, field
from typing import Any

from homeostat import automation, house

PRESENCE_ASPECTS = ("home", "sighted")
POSITION_ASPECTS = ("lat", "lon", "accuracy", "fixed_at")
# Tracker aspects this unit reads: presence evidence, a fix and liveness.
READ_ASPECTS = frozenset({*PRESENCE_ASPECTS, *POSITION_ASPECTS, "available"})


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
        (computed aspect, source name) to whether the source decided it.
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
    """

    def __init__(self, persons: list[house.Entity]):
        self.persons = persons
        self.trackers = {src.entity for p in persons for src in p.sources.values()}
        # tracker -> aspect -> last value
        self.readings: dict[str, dict[str, Any]] = {}
        # tracker -> its last complete fix, as it stood when `fixed_at` arrived
        self.fixes: dict[str, dict[str, Any]] = {}

    def seen(self, tracker: str, aspect: str, value: Any) -> list[house.Entity]:
        """Record one tracker reading and return the persons to evaluate again.

        A tracker publishes `fixed_at` after the other aspects of a fix, so
        its arrival completes the fix (`commit`). A `lat`, `lon` or
        `accuracy` alone concerns nobody yet: evaluated then, it would
        pair with the rest of the previous fix.
        """
        if tracker not in self.trackers or aspect not in READ_ASPECTS:
            return []
        self.readings.setdefault(tracker, {})[aspect] = value
        if aspect in ("lat", "lon", "accuracy"):
            return []
        if aspect == "fixed_at":
            self.commit(tracker)
        return [p for p in self.persons if any(s.entity == tracker for s in p.sources.values())]

    def commit(self, tracker: str) -> None:
        """Take a tracker's current position aspects as its latest fix."""
        reading = self.readings.get(tracker, {})
        if any(number(reading.get(a)) is None for a in ("lat", "lon", "fixed_at")):
            return
        self.fixes[tracker] = {a: reading[a] for a in POSITION_ASPECTS if a in reading}

    def usable(self, tracker: str) -> bool:
        """Whether a tracker's presence evidence counts: it is not reported unavailable."""
        return self.readings.get(tracker, {}).get("available") is not False

    def evaluate(self, person: house.Entity, max_accuracy_m: float) -> Outcome:
        """Apply the persons rule to one person."""
        out = Outcome()
        self._presence(person, out)
        self._position(person, max_accuracy_m, out)
        trackers = {src.entity for src in person.sources.values()}
        reported = [
            self.readings[t]["available"] for t in trackers
            if isinstance(self.readings.get(t, {}).get("available"), bool)
        ]
        if any(reported):
            out.values["available"] = True
        elif reported and len(reported) == len(trackers):
            out.values["available"] = False
        return out

    def _presence(self, person: house.Entity, out: Outcome) -> None:
        says: dict[str, bool] = {}  # source name -> the usable tracker's boolean
        geofenced = False
        for name, src in person.sources.items():
            if src.aspect not in PRESENCE_ASPECTS:
                continue
            geofenced = geofenced or src.aspect == "home"
            value = self.readings.get(src.entity, {}).get(src.aspect)
            if self.usable(src.entity) and isinstance(value, bool):
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
        decided: list[str] = []
        if home:
            decided = home
            out.decision = f"home: {', '.join(home)} says so"
        elif left:
            decided = left
            out.decision = f"away: {', '.join(left)} left the geofence"
        elif not geofenced and sighted and sorted(sighted) == unseen:
            decided = unseen
            out.decision = f"away: no geofence, and {', '.join(unseen)} has not seen the device"
        else:
            out.decision = "unchanged: no usable tracker says home or left"
        if decided:
            out.values["presence"] = bool(home)
        out.values["presence_valid"] = bool(decided)
        for name, src in sources.items():
            if src.aspect in PRESENCE_ASPECTS:
                out.used[("presence", name)] = name in decided

    def _position(self, person: house.Entity, max_accuracy_m: float, out: Outcome) -> None:
        best: tuple[float, str] | None = None
        for name, src in person.sources.items():
            if src.aspect != "lat":
                continue
            fix = self.fixes.get(src.entity)
            if fix is None:
                continue
            accuracy = number(fix.get("accuracy"))
            if accuracy is not None and accuracy > max_accuracy_m:
                continue
            fixed_at = float(fix["fixed_at"])
            if best is None or fixed_at > best[0]:
                best = (fixed_at, name)
        for name, src in person.sources.items():
            if src.aspect == "lat":
                for aspect in POSITION_ASPECTS:
                    out.used[(aspect, name)] = best is not None and best[1] == name
        if best is not None:
            out.values.update(self.fixes[person.sources[best[1]].entity])


class Unit:
    """The persons rule on the bus: catch-up, then one evaluation per change.

    Parameters
    ----------
    ctx : automation.Context
        The unit's context, or anything with `publish`, `source_used`,
        `decision` and `params.max_accuracy_m`.
    persons : list of house.Entity
        The person entities this unit binds.
    """

    def __init__(self, ctx, persons: list[house.Entity]):
        self.ctx = ctx
        self.persons = persons
        self.names = {p.name for p in persons}
        self.rule = Persons(persons)
        self.lock = threading.Lock()
        # (person, aspect) -> the value on the bus. Seeded from the mirror
        # during catch-up, so a restart republishes only what changed.
        self.published: dict[tuple[str, str], Any] = {}
        self.started = False

    def on_state(self, key: str, value: Any) -> None:
        """Take one state sample from the `trackers` subscription."""
        _, _, _room, entity, aspect = key.split("/")
        with self.lock:
            if entity in self.names:
                # The persons' own keys come back on this subscription.
                if not self.started:
                    self.published[(entity, aspect)] = value
                return
            concerned = self.rule.seen(entity, aspect, value)
            # During catch-up the mirror delivers one key at a time, in no
            # set order. Evaluating then could act on a `home` before the
            # same tracker's `available = false`, so the first evaluation
            # waits until every value is in (`start`).
            if self.started:
                for person in concerned:
                    self.apply(person)

    def start(self) -> None:
        """Evaluate every person once, after the catch-up."""
        with self.lock:
            self.started = True
            for tracker in self.rule.trackers:
                self.rule.commit(tracker)
            for person in self.persons:
                self.apply(person)

    def apply(self, person: house.Entity) -> None:
        """Evaluate one person and publish what changed."""
        ctx = self.ctx
        out = self.rule.evaluate(person, ctx.params.max_accuracy_m)
        for aspect, value in out.values.items():
            if self.published.get((person.name, aspect)) != value:
                ctx.publish("person", value, room=person.room, entity=person.name, aspect=aspect)
                self.published[(person.name, aspect)] = value
        for (computed, source), used in out.used.items():
            ctx.source_used(person.name, computed, source, used)
        ctx.decision(person.name, out.decision)


def main():
    ctx = automation.context()
    persons = [
        e for e in house.load_house().entities
        if e.owner == ctx.unit and e.capability == "person"
    ]
    unit = Unit(ctx, persons)
    ctx.subscribe("trackers", unit.on_state)
    unit.start()
    ctx.ready()
    ctx.run()


if __name__ == "__main__":
    main()
