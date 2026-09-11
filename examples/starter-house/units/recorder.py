# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "homeostat==0.11.4",
# ]
# ///
"""Recorder service: history end to end (see docs/design.md, step 5a).

Subscribes the key spaces its manifest declares and writes a SQLite store
named by [discovery].endpoint ("sqlite:<path>", relative to the house
root). NOT a naive bus mirror: state/cmd payloads are decoded and typed on
the way in — series identity is (class, entity, aspect), room is a tag, so
an entity move is a tag transition on a continuous series. Health and
config keys land raw in an events audit table; so does every cmd envelope
(alongside its unwrapped value in samples) — the "who" audit, askable via
home/history/events.

Timestamps are recorder receive time (µs, UTC), assigned before any
buffering, so a backend outage never distorts history. A failed flush
keeps samples in a bounded in-memory buffer (drop-oldest) and leaves
`backend-outage` / `backend-restored` events at home/health/{unit}/event;
each flush is one transaction on a connection opened per flush, so the
failure domain is "can I open and commit right now".

History reads go over the bus: a queryable on home/history/** answers two
shapes. GET home/history/{state|cmd}/{entity}/{aspect}?from=..;to=..;limit=..
replies one message per concrete series, a JSON array of {ts, room, value}
(from/to are RFC3339 timestamps with a UTC offset). GET home/history/events
?key=..;from=..;to=..;limit=.. replies one message, a JSON array of
{ts, key, payload} drawn from the events audit table — key is a
zenoh-style key expression (wildcards included) filtering which recorded
event keys come back, missing key means all of them; from/to here are
integer microseconds UTC, the recorder's own timestamp convention, unlike
the RFC3339 samples path. Both paths cap rows at limit, newest kept,
replied oldest-to-newest. GET home/history/stats replies one message
describing the store itself: file and freelist size, per-series row
counts and time bounds (keyed by history key, RFC3339 like the samples
path), the events table's count and bounds (integer µs, like the events
path) and the layout version — what an owner needs to see before
choosing a retention window.

Retention is two parameters, retain_samples_days and retain_events_days,
0 meaning forever (the default, so an upgrade never deletes history).
The writer thread purges rows older than the window hourly and whenever
a window changes, then returns the freed pages to the filesystem
(PRAGMA incremental_vacuum, what the file's auto_vacuum mode is for),
and leaves one `purge` health event per purge that deleted anything.
Retention is the only destructive operation in the store.

SQLite has no page checksums, so a disk returning corrupt data is silent
until a read happens to hit it. Every integrity_check_hours (default
daily, 0 disables) a checker thread runs PRAGMA integrity_check on a
read-only connection and leaves `integrity-ok` with the duration or
`integrity-failed` with what SQLite reported at home/health/{unit}/event.
"""

import datetime
import json
import signal
import sqlite3
import threading
import time
import tomllib
from collections import deque
from pathlib import Path

import zenoh

from homeostat import house, keys, session
from homeostat.params import LiveParams

BUFFER_LIMIT = 10_000
RETRY_S = 1.0
PURGE_INTERVAL_S = 3600.0
PARAM_DEFAULTS = {
    "retain_samples_days": 0.0,
    "retain_events_days": 0.0,
    "integrity_check_hours": 24.0,
}
DEFAULT_QUERY_LIMIT = 1000
DEFAULT_EVENTS_LIMIT = 500
EVENTS_KEY = zenoh.KeyExpr("home/history/events")
STATS_KEY = zenoh.KeyExpr("home/history/stats")

# Store layout, stamped in PRAGMA user_version. Version 0 was one wide
# samples table repeating class/room/entity/aspect/kind as TEXT on every
# row (and again in its index); measured at ~113 bytes a row against ~25
# for this layout, which is what bounds the file between retentions.
STORE_VERSION = 1

SCHEMA = """
CREATE TABLE IF NOT EXISTS series (
  id INTEGER PRIMARY KEY,
  class TEXT NOT NULL,
  entity TEXT NOT NULL,
  aspect TEXT NOT NULL,
  UNIQUE (class, entity, aspect)
);
CREATE TABLE IF NOT EXISTS rooms (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL UNIQUE
);
CREATE TABLE IF NOT EXISTS samples (
  series_id INTEGER NOT NULL REFERENCES series (id),
  ts INTEGER NOT NULL,
  room_id INTEGER NOT NULL REFERENCES rooms (id),
  kind INTEGER NOT NULL,
  value NOT NULL,
  PRIMARY KEY (series_id, ts)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS events (
  ts INTEGER NOT NULL,
  key TEXT NOT NULL,
  payload TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS events_key ON events (key, ts);
CREATE INDEX IF NOT EXISTS events_ts ON events (ts);
CREATE VIEW IF NOT EXISTS history AS
  SELECT ts, class, rooms.name AS room, entity, aspect,
         CASE kind WHEN 0 THEN 'bool' WHEN 1 THEN 'number' ELSE 'string' END AS kind,
         value
  FROM samples
  JOIN series ON series.id = samples.series_id
  JOIN rooms ON rooms.id = samples.room_id;
"""

# samples.kind codes, in the order the history view spells them out; the
# wire and the docs keep the names.
KINDS = ("bool", "number", "string")


def now_us() -> int:
    return time.time_ns() // 1_000


def iso_utc(us: int) -> str:
    return datetime.datetime.fromtimestamp(us / 1e6, tz=datetime.timezone.utc).isoformat(
        timespec="microseconds"
    )


class Params(LiveParams):
    """The retention windows and the integrity-check interval from
    home/config/{unit}/*, live; a change wakes the threads that use them
    so it applies at once."""

    def __init__(self, sess: session.UnitSession, on_change):
        self._on_change = on_change
        super().__init__(sess, PARAM_DEFAULTS)

    def _on_config(self, sample) -> None:
        super()._on_config(sample)
        self._on_change()

    @property
    def retain_samples_days(self) -> float:
        return self.get("retain_samples_days")

    @property
    def retain_events_days(self) -> float:
        return self.get("retain_events_days")

    @property
    def integrity_check_hours(self) -> float:
        return self.get("integrity_check_hours")


class IntegrityChecker:
    """Runs PRAGMA integrity_check on its own read-only connection every
    integrity_check_hours, the first one an interval after start so a
    restart loop never hammers a large file. Read-only, so in WAL mode it
    never blocks the writer."""

    def __init__(self, db_path: Path, sess: session.UnitSession):
        self.db_path = db_path
        self.sess = sess
        self.params: Params | None = None  # set once the params exist
        self.wake = threading.Event()
        self.stopping = False
        # A daemon: a check mid-run on a large file must not hold up the
        # unit's exit past its grace, and a read-only check abandoned at
        # exit harms nothing.
        self.thread = threading.Thread(target=self._run, name="integrity", daemon=True)

    def stop(self) -> None:
        self.stopping = True
        self.wake.set()

    def _run(self) -> None:
        last = time.monotonic()
        while not self.stopping:
            interval = self.params.integrity_check_hours * 3600 if self.params else 0
            if interval <= 0:
                self.wake.wait()
                self.wake.clear()
                continue
            remaining = last + interval - time.monotonic()
            if remaining > 0:
                self.wake.wait(timeout=remaining)
                self.wake.clear()
                continue
            self.check()
            last = time.monotonic()

    def check(self) -> None:
        started = time.monotonic()
        try:
            conn = sqlite3.connect(f"file:{self.db_path}?mode=ro", uri=True, timeout=2.0)
            try:
                lines = [row[0] for row in conn.execute("PRAGMA integrity_check")]
            finally:
                conn.close()
        except sqlite3.Error as err:
            self.sess.health_event("integrity-failed", errors=[str(err)])
            return
        duration_s = round(time.monotonic() - started, 3)
        if lines == ["ok"]:
            self.sess.health_event("integrity-ok", duration_s=duration_s)
        else:
            self.sess.health_event("integrity-failed", errors=lines[:5], duration_s=duration_s)


class Writer:
    """Single writer thread draining a bounded queue, one transaction per
    flush. Failed batches stay pending and retry on new samples or a timer.
    Retention purges run here too, so they serialise with flushes."""

    def __init__(self, db_path: Path, sess: session.UnitSession):
        self.db_path = db_path
        self.sess = sess
        self.cond = threading.Condition()
        self.queue: deque = deque()
        self.stopping = False
        self.purge_due = False
        # The checker before the params: a config sample can arrive the
        # moment the params subscription exists, and the change handler
        # wakes both.
        self.checker = IntegrityChecker(db_path, sess)
        self.params = Params(sess, self._on_param_change)
        self.checker.params = self.params
        self.thread = threading.Thread(target=self._run, name="writer")

    def _on_param_change(self) -> None:
        self.request_purge()
        self.checker.wake.set()

    def enqueue(self, table: str, row: tuple) -> None:
        with self.cond:
            self.queue.append((table, row))
            self.cond.notify()

    def request_purge(self) -> None:
        with self.cond:
            self.purge_due = True
            self.cond.notify()

    def stop(self) -> None:
        """Requests a final flush attempt and waits for the thread."""
        with self.cond:
            self.stopping = True
            self.cond.notify()
        self.thread.join(timeout=5)

    def _run(self) -> None:
        pending: list = []
        outage = False
        dropped = 0
        next_purge = time.monotonic() + PURGE_INTERVAL_S
        while True:
            with self.cond:
                while not self.queue and not pending and not self.stopping:
                    if self.purge_due or time.monotonic() >= next_purge:
                        break
                    self.cond.wait(timeout=max(0.0, next_purge - time.monotonic()))
                if not self.queue and not pending and self.stopping:
                    return
                if not self.queue and not pending:
                    self.purge_due = False
                    next_purge = time.monotonic() + PURGE_INTERVAL_S
                    purge = True
                else:
                    purge = False
                pending.extend(self.queue)
                self.queue.clear()
                stopping = self.stopping
            if purge:
                self._purge()
                continue
            overflow = len(pending) - BUFFER_LIMIT
            if overflow > 0:
                del pending[:overflow]
                dropped += overflow
            try:
                self._flush(pending)
            except sqlite3.Error as err:
                if not outage:
                    outage = True
                    self.sess.health_event("backend-outage", error=str(err))
                if stopping:
                    return
                with self.cond:
                    self.cond.wait(timeout=RETRY_S)
                continue
            if outage:
                outage = False
                self.sess.health_event(
                    "backend-restored", flushed=len(pending), dropped=dropped
                )
                dropped = 0
            pending.clear()

    def _flush(self, rows: list) -> None:
        conn = sqlite3.connect(self.db_path, timeout=2.0)
        try:
            with conn:
                self._intern(conn, rows)
                conn.executemany(
                    "INSERT OR IGNORE INTO samples VALUES ("
                    "  (SELECT id FROM series WHERE class = ? AND entity = ? AND aspect = ?),"
                    "  ?, (SELECT id FROM rooms WHERE name = ?), ?, ?)",
                    [
                        (space, entity, aspect, ts, room, kind, value)
                        for ts, space, room, entity, aspect, kind, value in (
                            row for table, row in rows if table == "samples"
                        )
                    ],
                )
                conn.executemany(
                    "INSERT INTO events VALUES (?, ?, ?)",
                    [row for table, row in rows if table == "events"],
                )
        finally:
            conn.close()

    def _purge(self) -> None:
        """Deletes rows older than each table's window and returns the
        pages to the filesystem. One event per purge that deleted
        anything; a purge that finds nothing to delete is silent, so
        retention never fills the events table with its own bookkeeping."""
        windows = {
            "samples": self.params.retain_samples_days,
            "events": self.params.retain_events_days,
        }
        if all(days <= 0 for days in windows.values()):
            return
        now = now_us()
        cutoffs = {
            table: now - int(days * 86_400 * 1_000_000)
            for table, days in windows.items()
            if days > 0
        }
        deleted = {"samples": 0, "events": 0}
        try:
            conn = sqlite3.connect(self.db_path, timeout=2.0)
            try:
                if "samples" in cutoffs:
                    # Per series, so the delete is a range on the primary
                    # key (series_id, ts) rather than a scan of the whole
                    # table, and one transaction per series keeps the
                    # writer's lock short even on a first purge of years.
                    for (series_id,) in conn.execute("SELECT id FROM series").fetchall():
                        with conn:
                            deleted["samples"] += conn.execute(
                                "DELETE FROM samples WHERE series_id = ? AND ts < ?",
                                (series_id, cutoffs["samples"]),
                            ).rowcount
                if "events" in cutoffs:
                    with conn:
                        deleted["events"] = conn.execute(
                            "DELETE FROM events WHERE ts < ?", (cutoffs["events"],)
                        ).rowcount
                if not any(deleted.values()):
                    return
                before = conn.execute("PRAGMA freelist_count").fetchone()[0]
                conn.execute("PRAGMA incremental_vacuum")
                after = conn.execute("PRAGMA freelist_count").fetchone()[0]
            finally:
                conn.close()
        except sqlite3.Error as err:
            self.sess.health_event("purge-failed", error=str(err))
            return
        self.sess.health_event(
            "purge",
            samples=deleted["samples"],
            events=deleted["events"],
            pages_freed=before - after,
        )

    def _intern(self, conn: sqlite3.Connection, rows: list) -> None:
        """Ensures every series and room the batch names has an id, so the
        per-row inserts are id lookups."""
        conn.executemany(
            "INSERT OR IGNORE INTO series (class, entity, aspect) VALUES (?, ?, ?)",
            {(r[1], r[3], r[4]) for t, r in rows if t == "samples"},
        )
        conn.executemany(
            "INSERT OR IGNORE INTO rooms (name) VALUES (?)",
            {(r[2],) for t, r in rows if t == "samples"},
        )


def typed(value):
    """(kind, stored value) for a scalar JSON value, None for non-scalars."""
    if isinstance(value, bool):
        return "bool", int(value)
    if isinstance(value, (int, float)):
        return "number", value
    if isinstance(value, str):
        return "string", value
    return None


def decode(kind: int, value):
    return bool(value) if KINDS[kind] == "bool" else value


# A seeded row is skipped when the store already holds the series at or
# after the value's time, give or take this: the live row's stamp is the
# recorder's receipt, the mirror's age counts from the core's, and the two
# sit milliseconds apart on the same host.
SEED_TOLERANCE_US = 500_000


class Recorder:
    def __init__(self, db_path: Path, sess: session.UnitSession):
        self.db_path = db_path
        self.sess = sess
        self.writer = Writer(db_path, sess)
        # State keys a live sample has reached since subscribing; the seed
        # skips them (a live sample is newer than any mirrored value).
        # Tracked only until the seed has run.
        self._live: set[str] | None = set()
        self._live_lock = threading.Lock()

    def record(self, sample: zenoh.Sample) -> None:
        ts = now_us()
        key = str(sample.key_expr)
        parts = key.split("/")
        if len(parts) > 1 and parts[1] in ("state", "cmd"):
            if parts[1] == "state":
                with self._live_lock:
                    if self._live is not None:
                        self._live.add(key)
            self._record_sample(ts, key, parts, sample)
        else:
            payload = sample.payload.to_bytes().decode("utf-8", errors="replace")
            self.writer.enqueue("events", (ts, key, payload))

    def seed(self, exprs: list[str]) -> None:
        """Catch-up from the core's state mirror (#60): whatever was
        published before this incarnation subscribed — a unit's start
        publish, a transition during a restart — is otherwise never
        recorded, and a rarely-changing aspect can have no history at all.
        Subscribe, then get, merge, as the SDK does for automations.

        A mirrored value can be arbitrarily old, so the row is stamped at
        the value's own time (now less the mirror's age), never at recorder
        start: a sample asserts an observation at its stamp. A series the
        store already holds at or after that time (a recorder-only restart,
        the live row written before it went down) is left alone. State
        only: commands, health and config land in the events audit, and a
        mirrored current value is not an event."""
        replies = [r for expr in exprs for r in self.sess.get_json_aged(expr)]
        with self._live_lock:
            live, self._live = self._live, None
        conn = sqlite3.connect(f"file:{self.db_path}?mode=ro", uri=True, timeout=2.0)
        try:
            for key, value, age_s in replies:
                parts = key.split("/")
                if len(parts) < 5 or parts[1] != "state" or key in live:
                    continue
                kind_value = typed(value)
                if kind_value is None:
                    continue  # the live path reports these; a seed stays quiet
                ts = now_us() - int(age_s * 1_000_000)
                entity, aspect = parts[3], "/".join(parts[4:])
                latest = conn.execute(
                    "SELECT MAX(ts) FROM samples WHERE series_id ="
                    " (SELECT id FROM series WHERE class = 'state' AND entity = ? AND aspect = ?)",
                    (entity, aspect),
                ).fetchone()[0]
                if latest is not None and latest >= ts - SEED_TOLERANCE_US:
                    continue
                kind, stored = kind_value
                self.writer.enqueue(
                    "samples", (ts, "state", parts[2], entity, aspect, KINDS.index(kind), stored)
                )
        finally:
            conn.close()

    def _record_sample(self, ts, key, parts, sample) -> None:
        if len(parts) < 5:
            self.sess.health_event("drop", reason="off-schema-key", key=key)
            return
        try:
            payload = json.loads(sample.payload.to_bytes())
        except ValueError:
            self.sess.health_event("drop", reason="malformed-payload", key=key)
            return
        if parts[1] == "cmd":
            try:
                value = keys.parse_cmd_envelope(payload)
            except ValueError:
                self.sess.health_event("drop", reason="invalid-command", key=key)
                return
        else:
            value = payload
        kind_value = typed(value)
        if kind_value is None:
            self.sess.health_event("drop", reason="non-scalar", key=key)
            return
        kind, stored = kind_value
        row = (ts, parts[1], parts[2], parts[3], "/".join(parts[4:]), KINDS.index(kind), stored)
        self.writer.enqueue("samples", row)
        if parts[1] == "cmd":
            # The "who" audit design.md anticipated: the full envelope
            # (value, priority, actor) lands in events alongside the
            # unwrapped value in samples.
            raw = sample.payload.to_bytes().decode("utf-8", errors="replace")
            self.writer.enqueue("events", (ts, key, raw))

    def answer(self, query: zenoh.Query) -> None:
        asked = zenoh.KeyExpr(str(query.key_expr))
        # Events only when the selector sits inside the events key: the two
        # paths disagree on from/to conventions (integer µs vs RFC3339), so
        # one query cannot serve both — and a wildcard like home/history/**
        # must fan out over the sample series, not silently drop them.
        if EVENTS_KEY.includes(asked):
            self._answer_events(query)
        elif STATS_KEY.includes(asked):
            self._answer_stats(query)
        else:
            self._answer_samples(query, asked)

    def _answer_samples(self, query: zenoh.Query, asked: zenoh.KeyExpr) -> None:
        try:
            from_us, to_us, limit = parse_params(str(query.parameters))
        except ValueError as err:
            query.reply_err(json.dumps(str(err)))
            return
        try:
            conn = sqlite3.connect(f"file:{self.db_path}?mode=ro", uri=True, timeout=2.0)
        except sqlite3.Error as err:
            query.reply_err(json.dumps(f"store unavailable: {err}"))
            return
        try:
            series = conn.execute(
                "SELECT id, class, entity, aspect FROM series"
            ).fetchall()
            for series_id, space, entity, aspect in series:
                series_key = f"home/history/{space}/{entity}/{aspect}"
                if not asked.intersects(zenoh.KeyExpr(series_key)):
                    continue
                rows = conn.execute(
                    "SELECT ts, room, kind, value FROM ("
                    "  SELECT ts, rooms.name AS room, kind, value FROM samples"
                    "  JOIN rooms ON rooms.id = samples.room_id"
                    "  WHERE series_id = ? AND ts >= ? AND ts <= ?"
                    "  ORDER BY ts DESC LIMIT ?"
                    ") ORDER BY ts ASC",
                    (series_id, from_us, to_us, limit),
                ).fetchall()
                payload = [
                    {"ts": iso_utc(ts), "room": room, "value": decode(kind, value)}
                    for ts, room, kind, value in rows
                ]
                query.reply(series_key, json.dumps(payload))
        except sqlite3.Error as err:
            query.reply_err(json.dumps(f"store unavailable: {err}"))
        finally:
            conn.close()

    def _answer_events(self, query: zenoh.Query) -> None:
        try:
            key_pattern, from_us, to_us, limit = parse_event_params(str(query.parameters))
        except ValueError as err:
            query.reply_err(json.dumps(str(err)))
            return
        try:
            conn = sqlite3.connect(f"file:{self.db_path}?mode=ro", uri=True, timeout=2.0)
        except sqlite3.Error as err:
            query.reply_err(json.dumps(f"store unavailable: {err}"))
            return
        try:
            if key_pattern is None:
                rows = conn.execute(
                    "SELECT ts, key, payload FROM events WHERE ts >= ? AND ts <= ?"
                    " ORDER BY ts DESC LIMIT ?",
                    (from_us, to_us, limit),
                ).fetchall()
            else:
                # Key filtering needs zenoh wildcard semantics, so the limit
                # can only apply after the Python-side match — but a LIKE on
                # the pattern's literal prefix bounds what gets materialized
                # (the default range is all of history).
                wildcards = [i for i, ch in enumerate(key_pattern) if ch in "*$"]
                prefix = key_pattern[: wildcards[0]] if wildcards else key_pattern
                like = (
                    prefix.replace("\\", "\\\\").replace("%", "\\%").replace("_", "\\_") + "%"
                )
                rows = conn.execute(
                    "SELECT ts, key, payload FROM events WHERE ts >= ? AND ts <= ?"
                    " AND key LIKE ? ESCAPE '\\' ORDER BY ts DESC",
                    (from_us, to_us, like),
                ).fetchall()
                pattern = zenoh.KeyExpr(key_pattern)
                rows = [row for row in rows if pattern.intersects(zenoh.KeyExpr(row[1]))]
            payload = [
                {"ts": ts, "key": key, "payload": event_payload(text)}
                for ts, key, text in reversed(rows[:limit])
            ]
            query.reply("home/history/events", json.dumps(payload))
        except sqlite3.Error as err:
            query.reply_err(json.dumps(f"store unavailable: {err}"))
        finally:
            conn.close()


    def _answer_stats(self, query: zenoh.Query) -> None:
        try:
            conn = sqlite3.connect(f"file:{self.db_path}?mode=ro", uri=True, timeout=2.0)
        except sqlite3.Error as err:
            query.reply_err(json.dumps(f"store unavailable: {err}"))
            return
        try:
            query.reply(str(STATS_KEY), json.dumps(store_stats(conn)))
        except sqlite3.Error as err:
            query.reply_err(json.dumps(f"store unavailable: {err}"))
        finally:
            conn.close()


def store_stats(conn: sqlite3.Connection) -> dict:
    """What is in the store: sizes from the pager, one aggregate per
    series and one for the events table."""
    page_size = conn.execute("PRAGMA page_size").fetchone()[0]
    page_count = conn.execute("PRAGMA page_count").fetchone()[0]
    freelist = conn.execute("PRAGMA freelist_count").fetchone()[0]
    series = {}
    for space, entity, aspect, rows, oldest, newest in conn.execute(
        "SELECT class, entity, aspect, COUNT(*), MIN(ts), MAX(ts) FROM samples"
        " JOIN series ON series.id = samples.series_id"
        " GROUP BY series_id ORDER BY class, entity, aspect"
    ):
        series[f"home/history/{space}/{entity}/{aspect}"] = {
            "rows": rows,
            "oldest": iso_utc(oldest),
            "newest": iso_utc(newest),
        }
    rows, oldest, newest = conn.execute("SELECT COUNT(*), MIN(ts), MAX(ts) FROM events").fetchone()
    return {
        "store_version": conn.execute("PRAGMA user_version").fetchone()[0],
        "file_bytes": page_count * page_size,
        "freelist_bytes": freelist * page_size,
        "series": series,
        "events": {"rows": rows, "oldest": oldest, "newest": newest},
    }


def event_payload(text: str):
    """Events are recorded raw (any bus client can put on these keys), so
    a non-JSON row must serve as its string — one poison row must never
    break every events query that reaches it."""
    try:
        return json.loads(text)
    except ValueError:
        return text


def split_selector(raw: str) -> dict[str, str]:
    """Splits a selector's parameters (zenoh's `a=1;b=2` grammar) into a
    dict. No URL decoding: RFC3339 offsets contain '+', which must stay
    literal."""
    params = {}
    for part in raw.split(";"):
        if not part:
            continue
        name, _, value = part.partition("=")
        params[name] = value
    return params


def parse_params(raw: str) -> tuple[int, int, int]:
    """from/to (RFC3339 with offset) and limit from a selector's parameters."""
    params = split_selector(raw)
    from_us, to_us, limit = 0, now_us(), DEFAULT_QUERY_LIMIT
    for bound in ("from", "to"):
        if bound not in params:
            continue
        try:
            dt = datetime.datetime.fromisoformat(params[bound])
        except ValueError:
            raise ValueError(f"{bound}: {params[bound]!r} is not RFC3339")
        if dt.tzinfo is None:
            raise ValueError(f"{bound}: {params[bound]!r} needs a UTC offset")
        us = int(dt.timestamp() * 1e6)
        if bound == "from":
            from_us = us
        else:
            to_us = us
    if "limit" in params:
        try:
            limit = int(params["limit"])
        except ValueError:
            raise ValueError(f"limit: {params['limit']!r} is not an integer")
        if limit < 1:
            raise ValueError(f"limit: {limit} is not positive")
    return from_us, to_us, limit


def parse_event_params(raw: str) -> tuple[str | None, int, int, int]:
    """key (a zenoh key expression filtering recorded event keys, wildcards
    included; None means all), from/to (integer microseconds UTC — the
    recorder's own timestamp convention, unlike the RFC3339 samples path)
    and limit, from a selector's parameters."""
    params = split_selector(raw)
    key = params.get("key")
    if key is not None:
        try:
            zenoh.KeyExpr(key)
        except zenoh.ZError as err:
            raise ValueError(f"key: {key!r} is not a valid key expression: {err}")
    from_us, to_us, limit = 0, now_us(), DEFAULT_EVENTS_LIMIT
    for bound in ("from", "to"):
        if bound not in params:
            continue
        try:
            us = int(params[bound])
        except ValueError:
            raise ValueError(f"{bound}: {params[bound]!r} is not an integer")
        if bound == "from":
            from_us = us
        else:
            to_us = us
    if "limit" in params:
        try:
            limit = int(params["limit"])
        except ValueError:
            raise ValueError(f"limit: {params['limit']!r} is not an integer")
        if limit < 1:
            raise ValueError(f"limit: {limit} is not positive")
    return key, from_us, to_us, limit


def init_store(db_path: Path) -> None:
    """Creates the store and its schema, or migrates an older layout in
    place. Must succeed before ready(): a recorder that never had a
    working store must not claim readiness."""
    db_path.parent.mkdir(parents=True, exist_ok=True)
    conn = sqlite3.connect(db_path)
    try:
        # Both pragmas persist in the file. Incremental auto_vacuum so a
        # future retention delete can return pages to the filesystem — it
        # only takes effect before the file's first page is written (or
        # across a VACUUM), which is why it is a schema decision and comes
        # first. WAL so the per-flush writer and read-only query
        # connections never block each other.
        conn.execute("PRAGMA auto_vacuum=INCREMENTAL")
        conn.execute("PRAGMA journal_mode=WAL")
        version = conn.execute("PRAGMA user_version").fetchone()[0]
        if version == 0 and has_v0_samples(conn):
            migrate_v0(conn)
        conn.executescript(SCHEMA)
        conn.execute(f"PRAGMA user_version={STORE_VERSION}")
        conn.commit()
    finally:
        conn.close()


def has_v0_samples(conn: sqlite3.Connection) -> bool:
    columns = {row[1] for row in conn.execute("PRAGMA table_info(samples)")}
    return "class" in columns


def migrate_v0(conn: sqlite3.Connection) -> None:
    """Version 0 -> 1: the wide samples table becomes series + rooms +
    narrow samples. One explicit transaction, so a crash mid-way leaves the
    version-0 file intact for the next start; the VACUUM after it is what
    switches an existing file to auto_vacuum."""
    conn.execute("BEGIN")
    conn.execute("ALTER TABLE samples RENAME TO samples_v0")
    for statement in SCHEMA.split(";\n"):
        if statement.strip():
            conn.execute(statement)
    conn.execute(
        "INSERT INTO series (class, entity, aspect)"
        " SELECT DISTINCT class, entity, aspect FROM samples_v0"
    )
    conn.execute("INSERT INTO rooms (name) SELECT DISTINCT room FROM samples_v0")
    conn.execute(
        "INSERT OR IGNORE INTO samples"
        " SELECT series.id, ts, rooms.id, CASE kind"
        + "".join(f" WHEN '{name}' THEN {code}" for code, name in enumerate(KINDS))
        + " END, value FROM samples_v0"
        " JOIN series ON series.class = samples_v0.class"
        "   AND series.entity = samples_v0.entity AND series.aspect = samples_v0.aspect"
        " JOIN rooms ON rooms.name = samples_v0.room"
    )
    conn.execute("DROP TABLE samples_v0")
    conn.execute("COMMIT")
    conn.execute("VACUUM")


def main():
    sess = session.connect()
    manifest = tomllib.loads(Path(f"units/{sess.unit}.toml").read_text())

    endpoint = house.load_endpoint(sess.unit)
    if not endpoint.startswith("sqlite:"):
        raise ValueError(f"recorder endpoint must be sqlite:<path>, got {endpoint}")
    db_path = Path(endpoint.removeprefix("sqlite:"))
    init_store(db_path)

    recorder = Recorder(db_path, sess)
    subs = [
        sess.subscribe(expr, recorder.record)
        for expr in manifest["bus"]["subscribes"].values()
    ]
    queryable = sess.declare_queryable(
        manifest["bus"]["publishes"]["history"]["key"], recorder.answer
    )
    recorder.writer.thread.start()
    recorder.writer.checker.thread.start()
    recorder.seed(
        [e for e in manifest["bus"]["subscribes"].values() if e.startswith("home/state/")]
    )
    sess.ready()

    stop = threading.Event()
    signal.signal(signal.SIGTERM, lambda *_: stop.set())
    signal.signal(signal.SIGINT, lambda *_: stop.set())
    stop.wait()

    for sub in subs:
        sub.undeclare()
    queryable.undeclare()
    recorder.writer.checker.stop()
    recorder.writer.stop()
    sess.close()


if __name__ == "__main__":
    main()
