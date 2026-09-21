# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "homeostat==0.14.0",
# ]
# ///
"""Recorder service: history end to end (see docs/design.md, step 5a).

Subscribes the key spaces its manifest declares and writes a SQLite store
named by [discovery].endpoint ("sqlite:<path>", relative to the house
root). NOT a naive bus mirror: state/cmd payloads are decoded and typed on
the way in (a non-finite number — NaN, Infinity, which Python's json
accepts — is dropped with a "non-finite" event like a non-scalar: SQLite
would bind it as NULL) — series identity is (class, entity, aspect), room
is a tag, so an entity move is a tag transition on a continuous series. A
row the schema still refuses at flush time (sqlite3.IntegrityError) is bad
data, not an outage: the batch lands without it and one "drop" event
names it, so a poison row can never stall the writer. Health and
config keys land raw in an events audit table; so does every cmd envelope
(alongside its unwrapped value in samples) — the "who" audit, askable via
home/history/events.

Forecasts (docs/design.md, Forecasts) are the one class that does NOT
ride samples, because a forecast point carries two times — when it was
said and when it is about — where a sample carries one, and keeping
superseded issues is the entire reason to store a forecast. They land in
`forecasts`, keyed (series_id, issued_ts, valid_ts) over the same
`series` and `rooms` tables, one row per point: the document is the unit
of issuance, the row is the unit of fact. A point's extent is stored
(`valid_end`, NULL for an instant, as an absent `d` is on the wire)
rather than derived from the next row, because the last point of a
horizon has no next row — the succession that `changes=1` relies on for
state is exactly what a forecast lacks. Rows are stamped with the
producer's own `issued`, never receipt: a delayed or replayed issue must
not read as a fresher opinion than it was.

Timestamps are recorder receive time (µs, UTC), assigned before any
buffering, so a backend outage never distorts history. A failed flush
keeps samples in a bounded in-memory buffer (drop-oldest) and leaves
`backend-outage` / `backend-restored` events at home/health/{unit}/event;
each flush is one transaction on a connection opened per flush, so the
failure domain is "can I open and commit right now".

History reads go over the bus: a queryable on home/history/** answers two
shapes. GET home/history/{state|cmd}/{entity}/{aspect}?from=..;to=..;limit=..
replies one message per concrete series, a JSON array of {ts, room, value}
(from/to are RFC3339 timestamps with a UTC offset). Two optional, mutually
exclusive shapes of the same path serve charts: bucket=<seconds> replies
one point per bucket (ts the bucket's start; a number's value is the mean
and the point carries min and max; a bool's or string's is the last value
seen), and changes=1 replies only the rows at which the value changed,
the window's first included — a state's runs. Both fold the whole window
before limit keeps the newest rows. GET home/history/forecast/{entity}/{aspect} answers in ISSUES rather than
rows, each in the wire's own shape so a consumer can hand it to the SDK's
decoder: at=<rfc3339> (the default, at now) is the forecast as it stood
then — the latest issue at or before that instant — and
valid_from=..;valid_to=.. is every issue that said something about that
window, each carrying only its overlapping points, which is what checking
a forecast against what happened reads. The two are exclusive, the window
needs both ends, and `limit` counts issues: an issue is the atom here, so
a reply is never cut across one.
GET home/history/events
?key=..;from=..;to=..;limit=.. replies one message, a JSON array of
{ts, key, payload} drawn from the events audit table — key is a
zenoh-style key expression (wildcards included) filtering which recorded
event keys come back, missing key means all of them; from/to here are
integer microseconds UTC, the recorder's own timestamp convention, unlike
the RFC3339 samples path. Both paths cap rows at limit, newest kept,
replied oldest-to-newest; a limit above MAX_QUERY_LIMIT is clamped to it,
and a from/to outside SQLite's signed 64-bit range is an error reply.
GET home/history/stats replies one message
describing the store itself: file and freelist size, per-series row
counts and time bounds (keyed by history key, RFC3339 like the samples
path), the events table's count and bounds (integer µs, like the events
path) and the layout version — what an owner needs to see before
choosing a retention window, each series also carrying the rate
(`rows_per_day`, null for a series too short to have one) that says
which of them is filling the file. The per-series aggregates are maintained on
the way in, so that reply is a read of one row per series and does not
slow down as the store grows; a store's size is answerable at the size
where the question gets asked.

Retention is three parameters — retain_samples_days,
retain_forecasts_days and retain_events_days — 0 meaning forever (the
default, so an upgrade never deletes history). Forecasts are purged on
issue time, since superseded issues are what grows; the knowing cost is
that a long-horizon forecast goes by its age even where part of its
horizon was never verified against anything.
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
import math
import signal
import sqlite3
import threading
import time
from collections import deque
from pathlib import Path

import tomllib
import zenoh
from homeostat import forecast, house, keys, session
from homeostat.params import LiveParams

BUFFER_LIMIT = 10_000
RETRY_S = 1.0
PURGE_INTERVAL_S = 3600.0
PARAM_DEFAULTS = {
    "retain_samples_days": 0.0,
    "retain_forecasts_days": 0.0,
    "retain_events_days": 0.0,
    "integrity_check_hours": 24.0,
}
DEFAULT_QUERY_LIMIT = 1000
DEFAULT_EVENTS_LIMIT = 500
# The most rows one reply carries; a larger limit is clamped, never refused.
MAX_QUERY_LIMIT = 10_000
# SQLite binds Python ints as signed 64-bit; anything else raises at bind
# time, outside sqlite3.Error, so the parse helpers refuse it first.
INT64_MIN, INT64_MAX = -(2**63), 2**63 - 1
EVENTS_KEY = zenoh.KeyExpr("home/history/events")
STATS_KEY = zenoh.KeyExpr("home/history/stats")
FORECAST_KEY = zenoh.KeyExpr("home/history/forecast/**")

# Store layout, stamped in PRAGMA user_version. Version 0 was one wide
# samples table repeating class/room/entity/aspect/kind as TEXT on every
# row (and again in its index); measured at ~113 bytes a row against ~25
# for this layout, which is what bounds the file between retentions.
# Version 1 kept every aggregate on the samples table, where COUNT/MIN/MAX
# per series have no index that answers them; version 2 carries them on
# series instead (see the tally trigger).
STORE_VERSION = 3

SCHEMA = """
CREATE TABLE IF NOT EXISTS series (
  id INTEGER PRIMARY KEY,
  class TEXT NOT NULL,
  entity TEXT NOT NULL,
  aspect TEXT NOT NULL,
  row_count INTEGER NOT NULL DEFAULT 0,
  oldest_ts INTEGER,
  newest_ts INTEGER,
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
CREATE TABLE IF NOT EXISTS forecasts (
  series_id INTEGER NOT NULL REFERENCES series (id),
  issued_ts INTEGER NOT NULL,
  valid_ts INTEGER NOT NULL,
  valid_end INTEGER,
  room_id INTEGER NOT NULL REFERENCES rooms (id),
  value NOT NULL,
  PRIMARY KEY (series_id, issued_ts, valid_ts)
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

# Per-series row count and time bounds, maintained on the way in so
# home/history/stats is a read of `series` (hundreds of rows) instead of
# SCAN samples (millions). The read matters more than its size suggests:
# ctx.restore polls stats to decide whether the recorder is answering at
# all, so a store big enough to push that scan past the query timeout
# resets every restoring latch to its code default -- the diagnostic
# degrading in proportion to the problem it diagnoses.
#
# A trigger rather than an UPDATE in the writer because four paths insert
# samples -- _flush, _flush_each, seed and the v0 migration -- and two of
# them are awkward to count in Python: _flush_each exists because some
# rows are refused by a constraint, and seed inserts rows dated in the
# past, so a batch's oldest row is not its contribution to oldest_ts. A
# trigger fires on exactly the rows that landed. It costs one UPDATE on a
# small, cached table per sample.
#
# Deletes are NOT triggered: retention is the only thing that deletes
# from this store, _purge already walks series one at a time with a
# rowcount in hand, and recomputing the bounds there is two seeks to the
# ends of a key range rather than an aggregate per deleted row.
#
# Kept out of SCHEMA because migrate_v0 splits that string on ";\n" to
# run it statement by statement inside its own transaction, and a trigger
# body contains one.
TRIGGERS = """
CREATE TRIGGER IF NOT EXISTS samples_tally AFTER INSERT ON samples BEGIN
  UPDATE series SET
    row_count = row_count + 1,
    oldest_ts = MIN(COALESCE(oldest_ts, NEW.ts), NEW.ts),
    newest_ts = MAX(COALESCE(newest_ts, NEW.ts), NEW.ts)
  WHERE id = NEW.series_id;
END;
CREATE TRIGGER IF NOT EXISTS forecasts_tally AFTER INSERT ON forecasts BEGIN
  UPDATE series SET
    row_count = row_count + 1,
    oldest_ts = MIN(COALESCE(oldest_ts, NEW.issued_ts), NEW.issued_ts),
    newest_ts = MAX(COALESCE(newest_ts, NEW.issued_ts), NEW.issued_ts)
  WHERE id = NEW.series_id;
END;
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
    def retain_forecasts_days(self) -> float:
        return self.get("retain_forecasts_days")

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
                try:
                    self._flush(pending)
                except sqlite3.IntegrityError:
                    # A row the schema refuses is bad data, not a dead
                    # backend: land the batch without it, one row at a
                    # time, and leave a trace per refused row.
                    for table, row, err in self._flush_each(pending):
                        self.sess.health_event(
                            "drop", reason="integrity-error", table=table, row=list(row), error=str(err)
                        )
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

    SAMPLES_INSERT = (
        "INSERT OR IGNORE INTO samples VALUES ("
        "  (SELECT id FROM series WHERE class = ? AND entity = ? AND aspect = ?),"
        "  ?, (SELECT id FROM rooms WHERE name = ?), ?, ?)"
    )
    EVENTS_INSERT = "INSERT INTO events VALUES (?, ?, ?)"
    # OR IGNORE, as for samples: a producer that re-publishes an issue
    # unchanged (a restart, a redelivery from the mirror) must not double
    # it, and (series_id, issued_ts, valid_ts) is exactly the identity of
    # "this issue's opinion about this instant".
    FORECASTS_INSERT = (
        "INSERT OR IGNORE INTO forecasts VALUES ("
        "  (SELECT id FROM series WHERE class = 'forecast' AND entity = ? AND aspect = ?),"
        "  ?, ?, ?, (SELECT id FROM rooms WHERE name = ?), ?)"
    )

    @staticmethod
    def _bind(table: str, row: tuple) -> tuple:
        if table == "samples":
            ts, space, room, entity, aspect, kind, value = row
            return (space, entity, aspect, ts, room, kind, value)
        if table == "forecasts":
            issued, valid, end, room, entity, aspect, value = row
            return (entity, aspect, issued, valid, end, room, value)
        return row

    def _flush(self, rows: list) -> None:
        conn = sqlite3.connect(self.db_path, timeout=2.0)
        try:
            with conn:
                self._intern(conn, rows)
                conn.executemany(
                    self.SAMPLES_INSERT,
                    [self._bind(table, row) for table, row in rows if table == "samples"],
                )
                conn.executemany(
                    self.FORECASTS_INSERT,
                    [self._bind(table, row) for table, row in rows if table == "forecasts"],
                )
                conn.executemany(
                    self.EVENTS_INSERT,
                    [row for table, row in rows if table == "events"],
                )
        finally:
            conn.close()

    def _flush_each(self, rows: list) -> list:
        """One transaction, one statement per row: the rows SQLite's
        constraints refuse are returned as (table, row, error) and the
        rest commit. Any other sqlite3.Error propagates as an outage."""
        refused = []
        conn = sqlite3.connect(self.db_path, timeout=2.0)
        try:
            with conn:
                self._intern(conn, rows)
                statements = {
                    "samples": self.SAMPLES_INSERT,
                    "forecasts": self.FORECASTS_INSERT,
                    "events": self.EVENTS_INSERT,
                }
                for table, row in rows:
                    sql = statements[table]
                    try:
                        conn.execute(sql, self._bind(table, row))
                    except sqlite3.IntegrityError as err:
                        refused.append((table, row, err))
        finally:
            conn.close()
        return refused

    def _purge(self) -> None:
        """Deletes rows older than each table's window and returns the
        pages to the filesystem. One event per purge that deleted
        anything; a purge that finds nothing to delete is silent, so
        retention never fills the events table with its own bookkeeping."""
        windows = {
            "samples": self.params.retain_samples_days,
            # Measured on issue time, not valid time: what grows without
            # bound is superseded issues, so bounding their age bounds the
            # table. The consequence to accept knowingly is that a
            # long-horizon forecast is purged by its age even where part
            # of its horizon has not happened yet and so was never
            # verified against anything.
            "forecasts": self.params.retain_forecasts_days,
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
        deleted = {"samples": 0, "forecasts": 0, "events": 0}
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
                            gone = conn.execute(
                                "DELETE FROM samples WHERE series_id = ? AND ts < ?",
                                (series_id, cutoffs["samples"]),
                            ).rowcount
                            if gone:
                                # The tally the insert trigger keeps, in
                                # the same transaction as the delete it
                                # describes. Both bounds are recomputed
                                # rather than only the old end: a series
                                # purged empty has no newest either, and
                                # each subquery is a seek to one end of
                                # this series' key range.
                                conn.execute(
                                    "UPDATE series SET row_count = row_count - ?,"
                                    " oldest_ts = (SELECT MIN(ts) FROM samples"
                                    "   WHERE series_id = ?),"
                                    " newest_ts = (SELECT MAX(ts) FROM samples"
                                    "   WHERE series_id = ?)"
                                    " WHERE id = ?",
                                    (gone, series_id, series_id, series_id),
                                )
                            deleted["samples"] += gone
                if "forecasts" in cutoffs:
                    # The same walk, on the other coordinate: the primary
                    # key is (series_id, issued_ts, valid_ts), so deleting
                    # by issue age is a range on its leading edge and one
                    # issue's points go together, which is what a purge of
                    # a forecast series means.
                    for (series_id,) in conn.execute("SELECT id FROM series").fetchall():
                        with conn:
                            gone = conn.execute(
                                "DELETE FROM forecasts WHERE series_id = ? AND issued_ts < ?",
                                (series_id, cutoffs["forecasts"]),
                            ).rowcount
                            if gone:
                                conn.execute(
                                    "UPDATE series SET row_count = row_count - ?,"
                                    " oldest_ts = (SELECT MIN(issued_ts) FROM forecasts"
                                    "   WHERE series_id = ?),"
                                    " newest_ts = (SELECT MAX(issued_ts) FROM forecasts"
                                    "   WHERE series_id = ?)"
                                    " WHERE id = ?",
                                    (gone, series_id, series_id, series_id),
                                )
                            deleted["forecasts"] += gone
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
            forecasts=deleted["forecasts"],
            events=deleted["events"],
            pages_freed=before - after,
        )

    def _intern(self, conn: sqlite3.Connection, rows: list) -> None:
        """Ensures every series and room the batch names has an id, so the
        per-row inserts are id lookups."""
        conn.executemany(
            "INSERT OR IGNORE INTO series (class, entity, aspect) VALUES (?, ?, ?)",
            {(r[1], r[3], r[4]) for t, r in rows if t == "samples"}
            | {("forecast", r[4], r[5]) for t, r in rows if t == "forecasts"},
        )
        conn.executemany(
            "INSERT OR IGNORE INTO rooms (name) VALUES (?)",
            {(r[2],) for t, r in rows if t == "samples"}
            | {(r[3],) for t, r in rows if t == "forecasts"},
        )


def typed(value):
    """(kind, stored value) for a scalar JSON value, None for non-scalars
    and non-finite numbers (SQLite would bind NaN as NULL)."""
    if isinstance(value, bool):
        return "bool", int(value)
    if isinstance(value, (int, float)):
        return ("number", value) if math.isfinite(value) else None
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
        elif len(parts) > 1 and parts[1] == "forecast":
            self._record_forecast(ts, key, parts, sample)
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
            reason = "non-finite" if isinstance(value, float) else "non-scalar"
            self.sess.health_event("drop", reason=reason, key=key)
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

    def _record_forecast(self, ts, key, parts, sample) -> None:
        """One issue becomes one row per point. The document is the unit
        of issuance; the row is the unit of fact (docs/design.md,
        Forecasts) — a scalar with its two coordinates, which is why it
        cannot ride `samples` and why it decomposes so plainly once it has
        its own table.

        `ts` — the recorder's receipt — is deliberately NOT what a row is
        stamped with. A forecast states its own `issued`, and that is the
        coordinate verification compares against; receipt time would make
        a replayed or delayed issue look like a fresher opinion than it
        is. Receipt still bounds nothing here, so a producer's clock is
        trusted for `issued` exactly as its values are trusted.

        A whole issue is refused or accepted together: a document with one
        bad point is a producer bug, and half-storing it would leave a
        forecast that reads as complete and is not."""
        if len(parts) < 5:
            self.sess.health_event("drop", reason="off-schema-key", key=key)
            return
        try:
            decoded = forecast.decode(sample.payload.to_bytes())
        except ValueError as error:
            self.sess.health_event(
                "drop", reason="malformed-payload", key=key, detail=str(error)
            )
            return
        issued_us = int(decoded.issued.timestamp() * 1_000_000)
        room, entity, aspect = parts[2], parts[3], "/".join(parts[4:])
        for point in decoded.points:
            valid_us = int(point.t.timestamp() * 1_000_000)
            end_us = (
                valid_us + int(point.d * 1_000_000) if point.d is not None else None
            )
            self.writer.enqueue(
                "forecasts",
                (issued_us, valid_us, end_us, room, entity, aspect, point.v),
            )

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
        elif FORECAST_KEY.includes(asked):
            # Same rule as events, for the same reason: the forecast path
            # takes different parameters and replies in a different shape
            # (issues, not rows), so a wildcard like home/history/** keeps
            # fanning out over the sample series alone rather than mixing
            # two answers nothing can read together.
            self._answer_forecasts(query, asked)
        else:
            self._answer_samples(query, asked)

    def _answer_samples(self, query: zenoh.Query, asked: zenoh.KeyExpr) -> None:
        try:
            from_us, to_us, limit, bucket_us, changes = parse_params(str(query.parameters))
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
                "SELECT id, class, entity, aspect FROM series WHERE class != 'forecast'"
            ).fetchall()
            for series_id, space, entity, aspect in series:
                series_key = f"home/history/{space}/{entity}/{aspect}"
                if not asked.intersects(zenoh.KeyExpr(series_key)):
                    continue
                if bucket_us or changes:
                    # A fold over the whole window, streamed off the cursor
                    # so a wide window costs time, never memory; the SQL
                    # LIMIT would cut the window before folding it.
                    rows = conn.execute(
                        "SELECT ts, rooms.name AS room, kind, value FROM samples"
                        " JOIN rooms ON rooms.id = samples.room_id"
                        " WHERE series_id = ? AND ts >= ? AND ts <= ?"
                        " ORDER BY ts ASC",
                        (series_id, from_us, to_us),
                    )
                    if bucket_us:
                        payload = bucketed(rows, bucket_us, limit)
                    else:
                        payload = [
                            {"ts": iso_utc(ts), "room": room, "value": decode(kind, value)}
                            for ts, room, kind, value in changes_only(rows, limit)
                        ]
                else:
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

    def _answer_forecasts(self, query: zenoh.Query, asked: zenoh.KeyExpr) -> None:
        """The two verification shapes, replying in issues rather than
        rows — an issue is the atom here, so it is also the unit `limit`
        counts and the unit a reply is never cut in half across.

        `at=<rfc3339>` (the default, at now) is the forecast as it stood
        then: the latest issue at or before that instant. `valid_from`/
        `valid_to` is every issue that said something about that window,
        each carrying just the points overlapping it — what "how wrong
        was it" reads, against the state the samples table holds for the
        same span.

        Each issue comes back in the wire's own shape, so a consumer can
        hand it straight to the SDK's decoder rather than learning a
        second spelling of the same thing."""
        try:
            at_us, from_us, to_us, limit = parse_forecast_params(str(query.parameters))
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
                "SELECT id, entity, aspect FROM series WHERE class = 'forecast'"
            ).fetchall()
            for series_id, entity, aspect in series:
                series_key = f"home/history/forecast/{entity}/{aspect}"
                if not asked.intersects(zenoh.KeyExpr(series_key)):
                    continue
                if at_us is not None:
                    rows = conn.execute(
                        "SELECT issued_ts, valid_ts, valid_end, value FROM forecasts"
                        " WHERE series_id = ? AND issued_ts = ("
                        "   SELECT MAX(issued_ts) FROM forecasts"
                        "   WHERE series_id = ? AND issued_ts <= ?)"
                        " ORDER BY valid_ts ASC",
                        (series_id, series_id, at_us),
                    ).fetchall()
                else:
                    # A point speaks for [valid_ts, valid_end); an instant
                    # speaks only for itself. The two want opposite
                    # comparators at the window's near edge — an instant
                    # AT `from` is inside it, an interval ENDING at `from`
                    # is not — so the predicate names both cases rather
                    # than picking one and being wrong half the time.
                    rows = conn.execute(
                        "SELECT issued_ts, valid_ts, valid_end, value FROM forecasts"
                        " WHERE series_id = ? AND valid_ts < ?"
                        "   AND (valid_end > ? OR (valid_end IS NULL AND valid_ts >= ?))"
                        " ORDER BY issued_ts ASC, valid_ts ASC",
                        (series_id, to_us, from_us, from_us),
                    ).fetchall()
                query.reply(series_key, json.dumps(as_issues(rows, limit)))
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
                # Newest first off the cursor, stopping at limit matches:
                # the prefix bounds the scan, never what is held in memory.
                cursor = conn.execute(
                    "SELECT ts, key, payload FROM events WHERE ts >= ? AND ts <= ?"
                    " AND key LIKE ? ESCAPE '\\' ORDER BY ts DESC",
                    (from_us, to_us, like),
                )
                pattern = zenoh.KeyExpr(key_pattern)
                rows = []
                for row in cursor:
                    if pattern.intersects(zenoh.KeyExpr(row[1])):
                        rows.append(row)
                        if len(rows) >= limit:
                            break
            payload = [
                {"ts": ts, "key": key, "payload": event_payload(text)}
                for ts, key, text in reversed(rows)
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


def rows_per_day(rows: int, oldest: int, newest: int) -> float | None:
    """A series' long-run write rate, or None for one too short to have
    one (a single row, or every row inside one microsecond). The reply
    already carries the three numbers this divides; it does the division
    because "which series is filling the file" is the question stats gets
    asked, and an owner choosing a retention window should not have to do
    arithmetic across 479 entries to answer it."""
    span = newest - oldest
    if span <= 0:
        return None
    return round(rows * 86_400 * 1_000_000 / span, 1)


def store_stats(conn: sqlite3.Connection) -> dict:
    """What is in the store: sizes from the pager, one aggregate per
    series and one for the events table. The per-series aggregates are
    read off `series`, where the insert trigger and _purge maintain them;
    `row_count > 0` keeps the reply what the old aggregate query made it,
    a series that has no rows right now having no entry."""
    page_size = conn.execute("PRAGMA page_size").fetchone()[0]
    page_count = conn.execute("PRAGMA page_count").fetchone()[0]
    freelist = conn.execute("PRAGMA freelist_count").fetchone()[0]
    series = {}
    for space, entity, aspect, rows, oldest, newest in conn.execute(
        "SELECT class, entity, aspect, row_count, oldest_ts, newest_ts FROM series"
        " WHERE row_count > 0 ORDER BY class, entity, aspect"
    ):
        series[f"home/history/{space}/{entity}/{aspect}"] = {
            "rows": rows,
            "oldest": iso_utc(oldest),
            "newest": iso_utc(newest),
            "rows_per_day": rows_per_day(rows, oldest, newest),
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


def parse_params(raw: str) -> tuple[int, int, int, int, bool]:
    """from/to (RFC3339 with offset), limit, bucket (µs, 0 = raw rows) and
    changes from a selector's parameters."""
    params = split_selector(raw)
    from_us, to_us, limit = 0, now_us(), DEFAULT_QUERY_LIMIT
    bucket_us, changes = 0, False
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
        limit = parse_limit(params["limit"])
    if "bucket" in params:
        bucket_us = parse_bucket(params["bucket"])
    if "changes" in params:
        if params["changes"] != "1":
            raise ValueError(f"changes: {params['changes']!r} is not 1")
        changes = True
    if bucket_us and changes:
        raise ValueError("bucket and changes are exclusive")
    if bucket_us and (to_us - from_us) // bucket_us > MAX_QUERY_LIMIT:
        # The fold walks the whole window: a bucket that would yield more
        # points than any reply carries is a scan nobody asked for.
        raise ValueError(
            f"bucket: {bucket_us // 1_000_000} s makes more than {MAX_QUERY_LIMIT}"
            " buckets over the window; widen it or narrow from/to"
        )
    return from_us, to_us, limit, bucket_us, changes


def as_issues(rows, limit: int) -> list:
    """Rows grouped into issues, in the wire's shape. Newest issues kept
    when there are more than `limit` — the samples path's convention, one
    level up: there it keeps the newest rows, here the newest issues,
    because half an issue is not a forecast."""
    issues: dict = {}
    for issued_ts, valid_ts, valid_end, value in rows:
        point = {"t": iso_utc(valid_ts), "v": value}
        if valid_end is not None:
            point["d"] = (valid_end - valid_ts) / 1_000_000
        issues.setdefault(issued_ts, []).append(point)
    kept = sorted(issues)[-limit:] if limit else sorted(issues)
    return [
        {"schema": forecast.SCHEMA, "issued": iso_utc(issued), "points": issues[issued]}
        for issued in kept
    ]


def parse_forecast_params(raw: str) -> tuple[int | None, int, int, int]:
    """`at` or `valid_from`+`valid_to` (RFC3339 with offset, as the
    samples path spells time), plus `limit` in issues.

    The two are exclusive and the range needs both ends: an unbounded
    verification window over a store of superseded issues is a scan
    nobody meant to ask for, and defaulting one end would be guessing
    which. Neither given means `at` now — the current forecast, which is
    what a bare read of the key should mean."""
    params = split_selector(raw)
    limit = parse_limit(params["limit"]) if "limit" in params else DEFAULT_QUERY_LIMIT
    ranged = "valid_from" in params or "valid_to" in params
    if "at" in params and ranged:
        raise ValueError("at and valid_from/valid_to are exclusive")
    if ranged:
        missing = [b for b in ("valid_from", "valid_to") if b not in params]
        if missing:
            raise ValueError(f"{missing[0]}: a verification window needs both ends")
        return None, rfc3339_us("valid_from", params["valid_from"]), rfc3339_us(
            "valid_to", params["valid_to"]
        ), limit
    at_us = rfc3339_us("at", params["at"]) if "at" in params else now_us()
    return at_us, 0, 0, limit


def rfc3339_us(name: str, raw: str) -> int:
    """An RFC3339 instant with an offset, in µs — the samples path's
    convention, named here so the forecast path cannot drift from it."""
    try:
        dt = datetime.datetime.fromisoformat(raw)
    except ValueError:
        raise ValueError(f"{name}: {raw!r} is not RFC3339")
    if dt.tzinfo is None:
        raise ValueError(f"{name}: {raw!r} needs a UTC offset")
    return int(dt.timestamp() * 1e6)


def parse_bucket(raw: str) -> int:
    """A positive bucket width in whole seconds, returned in µs."""
    try:
        seconds = int(raw)
    except ValueError:
        raise ValueError(f"bucket: {raw!r} is not an integer")
    if seconds < 1:
        raise ValueError(f"bucket: {seconds} is not positive")
    if seconds > INT64_MAX // 1_000_000:
        raise ValueError(f"bucket: {seconds} is out of range")
    return seconds * 1_000_000


def bucketed(rows, bucket_us: int, limit: int) -> list[dict]:
    """One point per bucket from ascending (ts, room, kind, value) rows —
    a cursor, folded as it streams, the newest `limit` kept: ts is the
    bucket's start, room the last row's; a number bucket's value is the
    mean and carries min and max, any other kind's is the last value seen
    (a run of bools or enum strings has no mean)."""
    points: deque[dict] = deque(maxlen=limit)
    point: dict | None = None
    for ts, room, kind, value in rows:
        start = ts - ts % bucket_us
        if point is None or point["_start"] != start:
            point = {"_start": start, "n": 0}
            points.append(point)
        point["room"], point["kind"], point["last"] = room, kind, value
        if KINDS[kind] == "number":
            point["n"] += 1
            point["sum"] = point.get("sum", 0.0) + value
            point["min"] = min(point.get("min", value), value)
            point["max"] = max(point.get("max", value), value)
    out = []
    for point in points:
        row = {"ts": iso_utc(point["_start"]), "room": point["room"]}
        if KINDS[point["kind"]] == "number" and point["n"]:
            row["value"] = point["sum"] / point["n"]
            row["min"], row["max"] = point["min"], point["max"]
        else:
            row["value"] = decode(point["kind"], point["last"])
        out.append(row)
    return out


def changes_only(rows, limit: int) -> list[tuple]:
    """The rows at which the value changed, the window's first included,
    the newest `limit` kept: a state's runs, for timelines. Repeats are
    kept in the store (each is a sighting) and collapsed here, on read,
    as the cursor streams."""
    out: deque[tuple] = deque(maxlen=limit)
    last = None
    for row in rows:
        if last is None or last[2:] != row[2:]:
            out.append(row)
        last = row
    return list(out)


def parse_limit(raw: str) -> int:
    """A positive row count, clamped to MAX_QUERY_LIMIT."""
    try:
        limit = int(raw)
    except ValueError:
        raise ValueError(f"limit: {raw!r} is not an integer")
    if limit < 1:
        raise ValueError(f"limit: {limit} is not positive")
    return min(limit, MAX_QUERY_LIMIT)


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
        if not INT64_MIN <= us <= INT64_MAX:
            raise ValueError(f"{bound}: {us} is outside the 64-bit timestamp range")
        if bound == "from":
            from_us = us
        else:
            to_us = us
    if "limit" in params:
        limit = parse_limit(params["limit"])
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
        existing = has_samples_table(conn)
        if version == 0 and has_v0_samples(conn):
            migrate_v0(conn)
        conn.executescript(SCHEMA)
        conn.executescript(TRIGGERS)
        if existing and version < 2:
            migrate_v1(conn)
        conn.execute(f"PRAGMA user_version={STORE_VERSION}")
        conn.commit()
    finally:
        conn.close()


def has_samples_table(conn: sqlite3.Connection) -> bool:
    """Whether this file already holds a store — a fresh one needs no
    migration, and its `series` is empty either way."""
    return bool(list(conn.execute("PRAGMA table_info(samples)")))


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


def migrate_v1(conn: sqlite3.Connection) -> None:
    """Version 1 -> 2: the per-series tally arrives, and an existing
    store's has to be counted once — at startup, where nothing is waiting
    on a query timeout, instead of on every stats read forever. Each
    aggregate is a correlated subquery on the clustered primary key
    rather than one GROUP BY over the table: the counts walk each
    series' key range, the bounds are seeks to its ends, and nothing
    depends on a SQLite newer than the schema already does (measured on a
    synthetic 4.8 M-row store: 0.13 s, against 0.62 s for the aggregate
    query this replaces). A store migrating straight from version 0
    already has the columns — they are in SCHEMA, which built its new
    tables — and only needs the count."""
    columns = {row[1] for row in conn.execute("PRAGMA table_info(series)")}
    if "row_count" not in columns:
        conn.execute("ALTER TABLE series ADD COLUMN row_count INTEGER NOT NULL DEFAULT 0")
        conn.execute("ALTER TABLE series ADD COLUMN oldest_ts INTEGER")
        conn.execute("ALTER TABLE series ADD COLUMN newest_ts INTEGER")
    conn.execute(
        "UPDATE series SET"
        " row_count = (SELECT COUNT(*) FROM samples WHERE series_id = series.id),"
        " oldest_ts = (SELECT MIN(ts) FROM samples WHERE series_id = series.id),"
        " newest_ts = (SELECT MAX(ts) FROM samples WHERE series_id = series.id)"
    )


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
