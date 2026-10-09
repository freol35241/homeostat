# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "homeostat==0.17.0",
# ]
# ///
"""Recorder service: writes the bus to a SQLite store and answers history reads.

The design is in docs/design.md#history-and-the-recorder, #retention and
#archives.

Store: [discovery].endpoint is "sqlite:<path>", relative to the house root.
The recorder subscribes every key expression in its manifest's
[bus.subscribes]. State and cmd values land typed in `samples`, forecasts
in `forecasts` (one row per point), and health, config and the full cmd
envelopes raw in `events`.

Reads: one queryable on the manifest's `history` key (home/history/**).
It answers home/history/{state|cmd}/{entity}/{aspect},
home/history/forecast/{entity}/{aspect}/{source}, home/history/events and
home/history/stats. Each path's parameters and reply shape are documented
on the method that answers it.

Live parameters (home/config/{unit}/*):

- retain_samples_days, retain_forecasts_days, retain_events_days: how long
  each table keeps rows. 0, the default, keeps them forever.
- archive_after_months: seal each month that closed more than this many
  months ago into archive/<store>-YYYY-MM.db beside the store. 0, the
  default, never archives.
- retain_archives_months: delete archive files whose month closed more
  than this many months ago. 0, the default, keeps them forever.
- integrity_check_hours: how often PRAGMA integrity_check runs. Default 24;
  0 disables it.

Health events at home/health/{unit}/event: `drop` (reasons off-schema-key,
malformed-payload, invalid-command, non-scalar, non-finite,
integrity-error), backend-outage, backend-restored, purge, purge-failed,
archive, archive-failed, archive-dropped, archive-drop-failed,
archive-misconfigured, integrity-ok, integrity-failed and query-failed.
"""

import datetime
import hashlib
import json
import math
import os
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
# Closed months move here, beside the store: data/history.db archives into
# data/archive/history-YYYY-MM.db.
ARCHIVE_DIR = "archive"
# Archive files one month may have and still be read in one pass (SQLite
# attaches at most ten databases besides the store, and a file being
# sealed takes one). A second file only exists for rows that reached a
# sealed month late, so more than a couple is already a sign something is
# wrong.
MAX_PARTS = 8
RETRY_S = 1.0
PURGE_INTERVAL_S = 3600.0
PARAM_DEFAULTS = {
    "retain_samples_days": 0.0,
    "retain_forecasts_days": 0.0,
    "retain_events_days": 0.0,
    "integrity_check_hours": 24.0,
    "archive_after_months": 0,
    "retain_archives_months": 0,
}
RETENTION_PARAMS = ("retain_samples_days", "retain_forecasts_days", "retain_events_days")
DEFAULT_QUERY_LIMIT = 1000
DEFAULT_EVENTS_LIMIT = 500
# The most rows one reply carries. A larger limit is clamped to it.
MAX_QUERY_LIMIT = 10_000
# SQLite binds Python ints as signed 64-bit; anything else raises at bind
# time, outside sqlite3.Error, so the parse helpers refuse it first.
INT64_MIN, INT64_MAX = -(2**63), 2**63 - 1
EVENTS_KEY = zenoh.KeyExpr("home/history/events")
STATS_KEY = zenoh.KeyExpr("home/history/stats")
FORECAST_KEY = zenoh.KeyExpr("home/history/forecast/**")

# Store layout, stamped in PRAGMA user_version. init_store migrates older
# files in place:
# - 0: one wide samples table with class/room/entity/aspect/kind as TEXT
#   on every row, about 113 bytes a row against about 25 for the
#   interned layout.
# - 1: series and rooms interned; per-series aggregates computed from
#   samples, where no index answers them.
# - 2: the aggregates live on `series` (see TRIGGERS).
# - 3: the forecasts table.
# - 4: `series.source`, because a forecast key carries a source and two
#   providers for one aspect are two series (docs/design.md#forecasts).
# - 5: forecast series with an empty source get LEGACY_SOURCE.
# - 6: the `archives` table.
STORE_VERSION = 6

# The source of forecast series migrated from before version 4. The store
# has no record of who issued those rows, so it uses a reserved name
# instead of inventing a provider. It cannot be empty: the source is a
# segment of the reply key, and an empty segment is not a valid key
# expression. The leading underscore marks it as not a unit name.
LEGACY_SOURCE = "_unknown"

# Series identity is (class, entity, aspect, source). The room is a
# per-row tag, so an entity that moves rooms stays one continuous series.
# `source` is empty for every class but forecast.

SCHEMA = """
CREATE TABLE IF NOT EXISTS series (
  id INTEGER PRIMARY KEY,
  class TEXT NOT NULL,
  entity TEXT NOT NULL,
  aspect TEXT NOT NULL,
  -- Who claims it. Empty for every class but `forecast`, and NOT NULL
  -- with that empty string as the sentinel rather than NULL: SQLite
  -- treats NULLs as distinct in a unique index, so a nullable column
  -- here would silently admit duplicate state series.
  source TEXT NOT NULL DEFAULT '',
  row_count INTEGER NOT NULL DEFAULT 0,
  oldest_ts INTEGER,
  newest_ts INTEGER,
  UNIQUE (class, entity, aspect, source)
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
-- The closed months sealed into archive files (see Writer._archive). A
-- row is written as 'sealing' before its file is, so a crash at any
-- point leaves a row that says what to finish or undo; only a 'sealed'
-- file is ever trusted to hold rows the hot file may then drop.
CREATE TABLE IF NOT EXISTS archives (
  file TEXT PRIMARY KEY,
  month TEXT NOT NULL,
  state TEXT NOT NULL,
  samples INTEGER,
  forecasts INTEGER,
  events INTEGER,
  bytes INTEGER,
  sha256 TEXT,
  sealed_ts INTEGER
);
CREATE VIEW IF NOT EXISTS history AS
  SELECT ts, class, rooms.name AS room, entity, aspect,
         CASE kind WHEN 0 THEN 'bool' WHEN 1 THEN 'number' ELSE 'string' END AS kind,
         value
  FROM samples
  JOIN series ON series.id = samples.series_id
  JOIN rooms ON rooms.id = samples.room_id;
"""

# Per-series row count and time bounds, maintained on insert so that
# home/history/stats reads `series` (hundreds of rows) instead of scanning
# samples (millions). ctx.restore polls stats to see whether the recorder
# is answering. If a large store pushed that scan past the query timeout,
# every restoring latch would fall back to its code default.
#
# A trigger instead of an UPDATE in the writer, because four paths insert
# samples: _flush, _flush_each, seed and the v0 migration. Two are hard to
# count in Python. _flush_each skips rows a constraint refuses, and seed
# inserts rows dated in the past, so a batch's oldest row is not its
# contribution to oldest_ts. The trigger fires only on rows that landed,
# at the cost of one UPDATE on a small cached table per sample.
#
# Deletes have no trigger. Only _purge and _prune delete samples or
# forecasts. They walk the series one at a time with a rowcount in hand,
# and recompute the bounds with two seeks to the ends of a key range
# instead of an aggregate per deleted row.
#
# Kept out of SCHEMA because migrate_v0 splits that string on ";\n" to run
# it statement by statement, and a trigger body contains ";\n".
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


def month_start_us(year: int, month: int) -> int:
    """Return the first microsecond of a calendar month, UTC.

    The month may run past 12 or below 1 and carries into the year.
    """
    year, month = year + (month - 1) // 12, (month - 1) % 12 + 1
    start = datetime.datetime(year, month, 1, tzinfo=datetime.timezone.utc)
    return int(start.timestamp()) * 1_000_000


def month_of(us: int) -> tuple[int, int]:
    when = datetime.datetime.fromtimestamp(us / 1e6, tz=datetime.timezone.utc)
    return when.year, when.month


def archive_boundary_us(now: int, months: int) -> int:
    """Return the stamp before which rows are in a month closed `months` ago.

    Rows stamped before this are in a month that closed more than `months`
    months ago: with 1, in October everything before September.
    """
    year, month = month_of(now)
    return month_start_us(year, month - months)


def month_label(month: tuple[int, int]) -> str:
    return f"{month[0]:04d}-{month[1]:02d}"


class ArchiveError(Exception):
    """An archive file that did not verify, or a month the pass cannot read."""


# The rows of one table in one month: samples and events by their stamp,
# forecasts by issue time, as retention measures them. Samples and
# forecasts go through `series` so each range is a seek on the primary key
# (series_id, ts...); events have an index on ts. CROSS JOIN fixes the
# join order in SQLite. The store is never ANALYZEd, and with a plain JOIN
# the planner scans all of samples and looks up each row's series. That
# took 0.2 s a month at 3 M rows, on the writer thread, for every month an
# idle latch keeps in the walk. Each clause ends in a WHERE so callers can
# append conditions with AND.
MONTH_ROWS = {
    "samples": "FROM main.series AS s CROSS JOIN main.samples AS r"
    " ON r.series_id = s.id AND r.ts >= ? AND r.ts < ? WHERE 1",
    "forecasts": "FROM main.series AS s CROSS JOIN main.forecasts AS r"
    " ON r.series_id = s.id AND r.issued_ts >= ? AND r.issued_ts < ? WHERE 1",
    "events": "FROM main.events AS r WHERE r.ts >= ? AND r.ts < ?",
}

# A row `r` is held by an archive file when the file has the same row,
# whole: same series, stamps, room and value. Events have no key, so all
# three of their columns are the identity, and an exact duplicate of an
# archived event (same microsecond, key and payload) counts as held.
HELD = {
    "samples": "x.series_id = r.series_id AND x.ts = r.ts AND x.room_id = r.room_id"
    " AND x.kind = r.kind AND x.value IS r.value",
    "forecasts": "x.series_id = r.series_id AND x.issued_ts = r.issued_ts"
    " AND x.valid_ts = r.valid_ts AND x.valid_end IS r.valid_end"
    " AND x.room_id = r.room_id AND x.value IS r.value",
    "events": "x.ts = r.ts AND x.key = r.key AND x.payload = r.payload",
}


def month_rows(table: str) -> str:
    return MONTH_ROWS[table]


def not_held(aliases: list, table: str) -> str:
    """Return a condition true for a row `r` none of the attached archive files holds."""
    if not aliases:
        return "1"
    return " AND ".join(
        f"NOT EXISTS (SELECT 1 FROM {alias}.{table} AS x WHERE {HELD[table]})"
        for alias in aliases
    )


def month_has_rows(conn: sqlite3.Connection, lo: int, hi: int) -> bool:
    return any(
        conn.execute(f"SELECT 1 {month_rows(table)} LIMIT 1", (lo, hi)).fetchone()
        for table in MONTH_ROWS
    )


def has_unsealed(conn: sqlite3.Connection, aliases: list, lo: int, hi: int) -> bool:
    return any(
        conn.execute(
            f"SELECT 1 {month_rows(table)} AND {not_held(aliases, table)} LIMIT 1",
            (lo, hi),
        ).fetchone()
        for table in MONTH_ROWS
    )


def closed_months(conn: sqlite3.Connection, boundary: int):
    """Yield every month from the hot file's oldest row up to the boundary.

    Months come oldest first. The tally gives the oldest sample or forecast
    without a scan, the events index the oldest event.
    """
    oldest = [
        value
        for value in (
            conn.execute("SELECT MIN(oldest_ts) FROM series").fetchone()[0],
            conn.execute("SELECT MIN(ts) FROM events").fetchone()[0],
        )
        if value is not None
    ]
    if not oldest:
        return
    year, month = month_of(min(oldest))
    while month_start_us(year, month) < boundary:
        yield year, month
        year, month = (year + 1, 1) if month == 12 else (year, month + 1)


def iso_utc(us: int) -> str:
    return datetime.datetime.fromtimestamp(us / 1e6, tz=datetime.timezone.utc).isoformat(
        timespec="microseconds"
    )


class Params(LiveParams):
    """The retention windows and the integrity-check interval, live.

    They come from home/config/{unit}/*; a change wakes the threads that use
    them so it applies at once.
    """

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

    @property
    def archive_after_months(self) -> int:
        return int(self.get("archive_after_months"))

    @property
    def retain_archives_months(self) -> int:
        return int(self.get("retain_archives_months"))


class IntegrityChecker:
    """Periodic PRAGMA integrity_check on its own read-only connection.

    SQLite has no page checksums, so a disk returning corrupt data goes
    unnoticed until a read hits it. The check runs every
    integrity_check_hours (0 disables it). The first run is one interval
    after start, so a restart loop does not re-check a large file each
    time. It is read-only, so in WAL mode it does not block the writer.
    Reports `integrity-ok` with the duration, or `integrity-failed` with
    the first lines SQLite reported.
    """

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
    """Single writer thread draining a bounded queue.

    Each flush is one transaction on a connection opened for that flush,
    so an outage is only "the store cannot be opened and committed right
    now". A failed flush keeps its rows pending, up to BUFFER_LIMIT with
    the oldest dropped first, and retries on new samples or every RETRY_S.
    The first failure reports `backend-outage`; the next success reports
    `backend-restored` with how many rows were flushed and dropped.
    Retention, archiving and archive deletion run on this thread too, so
    they serialise with flushes.
    """

    def __init__(self, db_path: Path, sess: session.UnitSession):
        self.db_path = db_path
        self.sess = sess
        self.cond = threading.Condition()
        self.queue: deque = deque()
        self.stopping = False
        self.purge_due = False
        # Set while closed months are waiting to be archived. A pass
        # archives one month, so on a first archive of years of data the
        # flushes run between months instead of waiting for all of it.
        self.archive_due = False
        # The settings problems last reported, so each is reported once per
        # change and not every hour.
        self._misconfigured: list[str] = []
        # The checker is created before the params because a config sample
        # can arrive as soon as the params subscription exists, and the
        # change handler wakes the checker.
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
        """Request a final flush attempt and wait for the thread."""
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
                    if self.purge_due or self.archive_due or time.monotonic() >= next_purge:
                        break
                    self.cond.wait(timeout=max(0.0, next_purge - time.monotonic()))
                if not self.queue and not pending and self.stopping:
                    return
                if not self.queue and not pending:
                    purge = self.purge_due or time.monotonic() >= next_purge
                    if purge:
                        self.purge_due = False
                        next_purge = time.monotonic() + PURGE_INTERVAL_S
                    archive = purge or self.archive_due
                    self.archive_due = False
                else:
                    purge = archive = False
                pending.extend(self.queue)
                self.queue.clear()
                stopping = self.stopping
            if purge or archive:
                # Retention runs first, so a row past its window is
                # deleted instead of archived and then deleted.
                if purge:
                    self._purge()
                    self._check_archive_settings()
                    self._drop_archives()
                if archive and self._archive():
                    with self.cond:
                        self.archive_due = True
                continue
            overflow = len(pending) - BUFFER_LIMIT
            if overflow > 0:
                del pending[:overflow]
                dropped += overflow
            try:
                try:
                    self._flush(pending)
                except sqlite3.IntegrityError:
                    # A row the schema refuses is bad data and the store
                    # is fine. Replay the batch one row at a time so the
                    # rest lands, and report each refused row, so one bad
                    # row cannot stall the writer.
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
    # OR IGNORE, as for samples: a producer that republishes an issue
    # unchanged (after a restart, or redelivered from the mirror) must not
    # store it twice. (series_id, issued_ts, valid_ts) identifies one
    # issue's value for one instant.
    FORECASTS_INSERT = (
        "INSERT OR IGNORE INTO forecasts VALUES ("
        "  (SELECT id FROM series WHERE class = 'forecast'"
        "     AND entity = ? AND aspect = ? AND source = ?),"
        "  ?, ?, ?, (SELECT id FROM rooms WHERE name = ?), ?)"
    )

    @staticmethod
    def _bind(table: str, row: tuple) -> tuple:
        if table == "samples":
            ts, space, room, entity, aspect, kind, value = row
            return (space, entity, aspect, ts, room, kind, value)
        if table == "forecasts":
            issued, valid, end, room, entity, aspect, source, value = row
            return (entity, aspect, source, issued, valid, end, room, value)
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
        """Insert the rows one statement each, returning those refused.

        One transaction, one statement per row: the rows SQLite's
        constraints refuse are returned as (table, row, error) and the
        rest commit. Any other sqlite3.Error propagates as an outage.
        """
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
        """Delete rows older than each table's window and free their pages.

        The writer runs this hourly and whenever a window changes. A window
        of 0 keeps that table forever. Retention is the only operation that
        deletes rows without keeping them elsewhere. Freed pages go back to
        the filesystem through PRAGMA incremental_vacuum, which is what the
        file's auto_vacuum mode allows. A purge that deleted anything
        reports one `purge` event; one that found nothing is silent, so
        retention does not fill the events table with its own bookkeeping.
        """
        windows = {
            "samples": self.params.retain_samples_days,
            # Measured on issue time: superseded issues are what grows, so
            # bounding their age bounds the table. The cost is that a
            # long-horizon forecast is purged by its age even where part
            # of its horizon has not happened yet and so was never
            # checked against anything.
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
                    # key (series_id, ts) instead of a table scan. One
                    # transaction per series keeps the write lock short,
                    # even on a first purge of years of data.
                    for (series_id,) in conn.execute("SELECT id FROM series").fetchall():
                        with conn:
                            gone = conn.execute(
                                "DELETE FROM samples WHERE series_id = ? AND ts < ?",
                                (series_id, cutoffs["samples"]),
                            ).rowcount
                            if gone:
                                # Update the tally the insert trigger
                                # keeps, in the same transaction as the
                                # delete. Both bounds are recomputed
                                # because a series purged empty has no
                                # newest either. Each subquery is a seek
                                # to one end of the series' key range.
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
                    # The same walk on issue time. The primary key is
                    # (series_id, issued_ts, valid_ts), so deleting by
                    # issue age is a range on its leading columns, and all
                    # points of one issue are deleted together.
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

    def _archive(self) -> bool:
        """Move one closed month out of the hot file into archive files.

        Archiving moves rows and deletes nothing. A month that closed more
        than archive_after_months ago is sealed into
        archive/<store>-YYYY-MM.db beside the store, and the rows the
        sealed file holds leave the hot file. The hot file stays one window
        deep while every observation is kept. home/history/** answers from
        the hot file only: archives are for people and tools (sqlite3,
        DuckDB ATTACH), and home/history/stats lists them. See
        docs/design.md#archives.

        Handles one month per call. Returns True when it did something, so
        the writer comes back for the next month once pending samples have
        flushed. Does nothing while archive_after_months is 0.
        """
        months = self.params.archive_after_months
        if months <= 0:
            return False
        archive_dir = self.db_path.parent / ARCHIVE_DIR
        # A failure is reported and the pass goes on, so one unreadable file
        # or one month that will not seal does not stop the other months.
        # ValueError and OverflowError come from a date out of range (an
        # absurd archive_after_months, a row stamped in year 1). They are
        # caught too, because an uncaught one kills the writer thread.
        failures = (sqlite3.Error, OSError, ArchiveError, ValueError, OverflowError)
        try:
            boundary = archive_boundary_us(now_us(), months)
            conn = sqlite3.connect(self.db_path, timeout=2.0)
        except failures as err:
            self.sess.health_event("archive-failed", month=None, error=str(err))
            return False
        try:
            self._finish_interrupted(conn, archive_dir, failures)
            try:
                months_due = list(closed_months(conn, boundary))
            except failures as err:
                self.sess.health_event("archive-failed", month=None, error=str(err))
                return False
            for month in months_due:
                try:
                    if self._archive_month(conn, archive_dir, month):
                        return True
                except failures as err:
                    self.sess.health_event(
                        "archive-failed", month=month_label(month), error=str(err)
                    )
            return False
        finally:
            conn.close()

    def _drop_archives(self) -> None:
        """Delete sealed archive files older than retain_archives_months.

        A file is deleted once its month closed more than
        retain_archives_months ago. Only whole files are deleted, since a
        sealed file is never rewritten. 0, the default, keeps them forever,
        and the retain_*_days windows do not apply to archives. This runs
        even when archive_after_months is 0, so archives made earlier still
        age out. The file is deleted before its record, so a crash between
        the two leaves a record of a missing file, which the next pass
        removes.
        """
        months = self.params.retain_archives_months
        if months <= 0:
            return
        archive_dir = self.db_path.parent / ARCHIVE_DIR
        dropped = []
        try:
            boundary = archive_boundary_us(now_us(), months)
            conn = sqlite3.connect(self.db_path, timeout=2.0)
            try:
                for name, month in conn.execute(
                    "SELECT file, month FROM archives WHERE state = 'sealed' ORDER BY month"
                ).fetchall():
                    year, number = (int(part) for part in month.split("-"))
                    if month_start_us(year, number) >= boundary:
                        continue
                    (archive_dir / name).unlink(missing_ok=True)
                    with conn:
                        conn.execute("DELETE FROM archives WHERE file = ?", (name,))
                    dropped.append(name)
            finally:
                conn.close()
        except (sqlite3.Error, OSError, ValueError, OverflowError) as err:
            self.sess.health_event("archive-drop-failed", dropped=dropped, error=str(err))
            return
        if dropped:
            self.sess.health_event("archive-dropped", files=dropped)

    def _check_archive_settings(self) -> None:
        """Report, once per change, when the archive settings undercut each other.

        A month is archived once it closed more than archive_after_months
        ago, so its first rows are by then up to archive_after_months + 1
        months old. A retention window shorter than that deletes them before
        they are archived. Archives kept no longer than archiving waits are
        deleted as soon as they are sealed. Both settings are allowed; each
        change that leaves such a conflict reports one
        `archive-misconfigured` event.
        """
        archive = self.params.archive_after_months
        problems = []
        if archive > 0:
            for param in RETENTION_PARAMS:
                days = self.params.get(param)
                if 0 < days < (archive + 1) * 31:
                    problems.append(
                        f"{param} ({days:g} days) deletes rows before"
                        f" archive_after_months ({archive}) archives them"
                    )
            keep = self.params.retain_archives_months
            if 0 < keep <= archive:
                problems.append(
                    f"retain_archives_months ({keep}) drops archives as soon as"
                    f" archive_after_months ({archive}) seals them"
                )
        if problems and problems != self._misconfigured:
            self.sess.health_event("archive-misconfigured", problems=problems)
        self._misconfigured = problems

    def _finish_interrupted(
        self, conn: sqlite3.Connection, archive_dir: Path, failures: tuple
    ) -> None:
        """Finish or discard the seals a crash interrupted.

        A 'sealing' row is a seal a crash interrupted. A file takes its
        final name only after it has verified, so a file under that name is
        complete and is recorded as sealed. Without one, nothing was pruned
        against it yet, so the half-written attempt is discarded and the
        month is sealed again from the hot rows.
        """
        for name, month in conn.execute(
            "SELECT file, month FROM archives WHERE state = 'sealing'"
        ).fetchall():
            final = archive_dir / name
            try:
                if final.exists():
                    self._record_sealed(conn, final)
                else:
                    Path(f"{final}.tmp").unlink(missing_ok=True)
                    with conn:
                        conn.execute("DELETE FROM archives WHERE file = ?", (name,))
            except failures as err:
                # Left as 'sealing', so nothing is pruned against it and
                # its name is not reused.
                self.sess.health_event("archive-failed", month=month, error=str(err))

    def _archive_month(self, conn: sqlite3.Connection, archive_dir: Path, month) -> bool:
        """Prune, seal and prune again one closed month; True when anything moved.

        Prunes what the month's sealed files already hold, seals what none
        of them does, and prunes that too.
        """
        label = month_label(month)
        lo, hi = month_start_us(*month), month_start_us(month[0], month[1] + 1)
        if not month_has_rows(conn, lo, hi):
            return False
        parts = [
            archive_dir / name
            for (name,) in conn.execute(
                "SELECT file FROM archives WHERE month = ? AND state = 'sealed'"
                " ORDER BY sealed_ts",
                (label,),
            )
        ]
        # A sealed file removed by hand holds nothing any more; what the
        # hot file still has of that month is sealed again.
        parts = [part for part in parts if part.exists()]
        if len(parts) > MAX_PARTS:
            raise ArchiveError(f"{label} has {len(parts)} archive files; at most {MAX_PARTS} are read")
        aliases = []
        sealed = None
        try:
            for i, part in enumerate(parts):
                conn.execute(f"ATTACH DATABASE ? AS part{i}", (str(part),))
                aliases.append(f"part{i}")
            pruned = self._prune(conn, aliases, lo, hi)
            if has_unsealed(conn, aliases, lo, hi):
                sealed = self._seal(conn, archive_dir, label, aliases, lo, hi)
                alias = f"part{len(aliases)}"
                conn.execute(f"ATTACH DATABASE ? AS {alias}", (str(archive_dir / sealed["file"]),))
                aliases.append(alias)
                pruned += self._prune(conn, [alias], lo, hi)
        finally:
            for alias in aliases:
                conn.execute(f"DETACH DATABASE {alias}")
        if not sealed and not pruned:
            return False
        conn.execute("PRAGMA incremental_vacuum")
        self.sess.health_event("archive", month=label, sealed=sealed, pruned=pruned)
        return True

    def _seal(self, conn, archive_dir: Path, label: str, aliases: list, lo: int, hi: int) -> dict:
        """Write the month's rows that no sealed file holds into a new archive file.

        The file is a plain SQLite file with the store's own schema, so
        `sqlite3` or a DuckDB ATTACH reads it like the store. It carries
        the store's series and room ids, so a row means the same thing in
        both. It is written under a temporary name and verified
        (integrity_check and row counts) before it takes its final name.
        """
        archive_dir.mkdir(parents=True, exist_ok=True)
        name = self._free_name(conn, archive_dir, label)
        with conn:
            conn.execute(
                "INSERT INTO archives (file, month, state) VALUES (?, ?, 'sealing')",
                (name, label),
            )
        final = archive_dir / name
        tmp = Path(f"{final}.tmp")
        tmp.unlink(missing_ok=True)
        init = sqlite3.connect(tmp)
        try:
            init.executescript(SCHEMA)
            init.executescript(TRIGGERS)
            init.execute(f"PRAGMA user_version={STORE_VERSION}")
            init.commit()
        finally:
            init.close()
        conn.execute("ATTACH DATABASE ? AS arch", (str(tmp),))
        try:
            with conn:
                conn.execute(
                    "INSERT INTO arch.series (id, class, entity, aspect, source)"
                    " SELECT id, class, entity, aspect, source FROM main.series"
                )
                conn.execute("INSERT INTO arch.rooms SELECT * FROM main.rooms")
                counts = {
                    table: conn.execute(
                        f"INSERT INTO arch.{table} SELECT r.* {month_rows(table)}"
                        f" AND {not_held(aliases, table)}",
                        (lo, hi),
                    ).rowcount
                    for table in ("samples", "forecasts", "events")
                }
        finally:
            conn.execute("DETACH DATABASE arch")
        check = sqlite3.connect(tmp)
        try:
            verdict = check.execute("PRAGMA integrity_check").fetchone()[0]
            found = {
                table: check.execute(f"SELECT COUNT(*) FROM {table}").fetchone()[0]
                for table in counts
            }
        finally:
            check.close()
        if verdict != "ok" or found != counts:
            raise ArchiveError(f"{name} did not verify: {verdict}, {found} != {counts}")
        with open(tmp, "rb") as handle:
            os.fsync(handle.fileno())
        os.replace(tmp, final)
        directory = os.open(archive_dir, os.O_RDONLY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
        return self._record_sealed(conn, final)

    def _record_sealed(self, conn: sqlite3.Connection, final: Path) -> dict:
        """Mark a verified, renamed file sealed and return its record.

        The record is the file's row counts, size and SHA-256. An archive
        is never written again, so a later check only needs the checksum.
        The file is made read-only on disk.
        """
        check = sqlite3.connect(f"file:{final}?mode=ro", uri=True)
        try:
            counts = {
                table: check.execute(f"SELECT COUNT(*) FROM {table}").fetchone()[0]
                for table in ("samples", "forecasts", "events")
            }
        finally:
            check.close()
        digest = hashlib.sha256()
        with open(final, "rb") as handle:
            for block in iter(lambda: handle.read(1 << 20), b""):
                digest.update(block)
        final.chmod(0o444)
        record = {
            "file": final.name,
            **counts,
            "bytes": final.stat().st_size,
            "sha256": digest.hexdigest(),
        }
        with conn:
            conn.execute(
                "UPDATE archives SET state = 'sealed', samples = ?, forecasts = ?,"
                " events = ?, bytes = ?, sha256 = ?, sealed_ts = ? WHERE file = ?",
                (
                    counts["samples"],
                    counts["forecasts"],
                    counts["events"],
                    record["bytes"],
                    record["sha256"],
                    now_us(),
                    final.name,
                ),
            )
        return record

    def _free_name(self, conn: sqlite3.Connection, archive_dir: Path, label: str) -> str:
        """Return the first free archive file name for a month.

        `<store>-YYYY-MM.db`, then `.2`, `.3`... for rows that reached a
        month after it was sealed, since a sealed file is never written
        again. A name already on disk that the store has no record of
        belongs to something else and is skipped.
        """
        taken = {name for (name,) in conn.execute("SELECT file FROM archives")}
        part = 1
        while True:
            suffix = "" if part == 1 else f".{part}"
            name = f"{self.db_path.stem}-{label}{suffix}.db"
            if name not in taken and not (archive_dir / name).exists():
                return name
            part += 1

    def _prune(self, conn: sqlite3.Connection, aliases: list, lo: int, hi: int) -> int:
        """Delete the month's hot rows that a sealed file holds; return how many.

        Rows are matched on the whole row. Each series' newest sample and
        newest forecast issue stay in the hot file as well, because
        restore, the seed and every latest-value read look for a series'
        last value there. A latch set months ago must still be found after
        a core restart. Works per series, like the purge, so each delete is
        a range on the primary key and the tally is updated in the same
        transaction.
        """
        if not aliases:
            return 0
        gone_total = 0
        for table, column in (("samples", "ts"), ("forecasts", "issued_ts")):
            for series_id, newest in conn.execute(
                f"SELECT DISTINCT r.series_id, s.newest_ts {month_rows(table)}",
                (lo, hi),
            ).fetchall():
                with conn:
                    gone = conn.execute(
                        f"DELETE FROM main.{table} AS r WHERE series_id = ?"
                        f" AND {column} >= ? AND {column} < ? AND {column} <> ?"
                        f" AND NOT ({not_held(aliases, table)})",
                        (series_id, lo, hi, newest),
                    ).rowcount
                    if gone:
                        conn.execute(
                            "UPDATE series SET row_count = row_count - ?,"
                            f" oldest_ts = (SELECT MIN({column}) FROM main.{table}"
                            "   WHERE series_id = ?)"
                            " WHERE id = ?",
                            (gone, series_id, series_id),
                        )
                gone_total += gone
        with conn:
            gone_total += conn.execute(
                "DELETE FROM main.events AS r WHERE ts >= ? AND ts < ?"
                f" AND NOT ({not_held(aliases, 'events')})",
                (lo, hi),
            ).rowcount
        return gone_total

    def _intern(self, conn: sqlite3.Connection, rows: list) -> None:
        """Ensure every series and room the batch names has an id.

        The per-row inserts are then id lookups.
        """
        conn.executemany(
            "INSERT OR IGNORE INTO series (class, entity, aspect, source)"
            " VALUES (?, ?, ?, ?)",
            {(r[1], r[3], r[4], "") for t, r in rows if t == "samples"}
            | {("forecast", r[4], r[5], r[6]) for t, r in rows if t == "forecasts"},
        )
        conn.executemany(
            "INSERT OR IGNORE INTO rooms (name) VALUES (?)",
            {(r[2],) for t, r in rows if t == "samples"}
            | {(r[3],) for t, r in rows if t == "forecasts"},
        )


def typed(value):
    """Return (kind, stored value) for a scalar JSON value, else None.

    None for non-scalars and non-finite numbers (SQLite would bind NaN as
    NULL).
    """
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
    """Routes received samples into the store's tables and seeds state from the mirror."""

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
        """Stamp a received sample and queue it for its table.

        The stamp is the recorder's receive time (µs, UTC), taken before
        any buffering, so a backend outage does not distort history. State
        and cmd go to `samples`, forecasts to `forecasts`. Every other key
        (health, config) lands raw in `events`, the audit table that
        home/history/events reads.
        """
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
        """Catch up from the core's state mirror.

        Values published before this process subscribed (a unit's start
        publish, a transition during a restart) would otherwise never be
        recorded, and a rarely changing aspect could have no history at
        all. The order is subscribe, then get, then merge, as the SDK does
        for automations.

        A mirrored value can be old, so the row is stamped at the value's
        own time (now less the mirror's age) and not at recorder start: a
        sample records an observation at its stamp. A series the store
        already holds at or after that time is left alone; that happens
        after a recorder-only restart, when the live row was written before
        it went down. Only state is seeded. Commands, health and config go
        to the events audit, and a mirrored current value is not an event.
        """
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
        """Decode, type and queue one state or cmd sample.

        A cmd is stored as its envelope's value. Anything that is not a
        finite JSON scalar is dropped with a `drop` event: non-scalar, or
        non-finite for NaN and Infinity, which Python's json accepts and
        SQLite would bind as NULL.
        """
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
            # The full envelope (value, priority, actor) also goes to
            # events, which records who sent the command.
            raw = sample.payload.to_bytes().decode("utf-8", errors="replace")
            self.writer.enqueue("events", (ts, key, raw))

    def _record_forecast(self, ts, key, parts, sample) -> None:
        """Record one forecast issue as one row per point.

        A forecast point has two times, when it was said and when it is
        about, where a sample has one, so forecasts have their own table
        (docs/design.md#forecasts). Superseded issues are kept, because
        checking them against what happened is why forecasts are stored.

        Rows are stamped with the producer's `issued`, not with `ts`, the
        recorder's receipt time. `issued` is what verification compares
        against, and receipt time would make a replayed or delayed issue
        look fresher than it is. The producer's clock is trusted for
        `issued` as its values are trusted.

        A point's extent is stored as `valid_end` (NULL for an instant, as
        an absent `d` is on the wire). It cannot be derived from the next
        row, because the last point of a horizon has none.

        An issue is accepted or refused as a whole. A document with one bad
        point is a producer bug, and storing half of it would leave a
        forecast that looks complete and is not.
        """
        # home/forecast/room/entity/aspect/source is six segments. An
        # aspect cannot span segments here, because the last one is the
        # source (docs/design.md#key-space).
        if len(parts) != 6:
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
        room, entity, aspect, source = parts[2], parts[3], parts[4], parts[5]
        for point in decoded.points:
            valid_us = int(point.t.timestamp() * 1_000_000)
            end_us = (
                valid_us + int(point.d * 1_000_000) if point.d is not None else None
            )
            self.writer.enqueue(
                "forecasts",
                (issued_us, valid_us, end_us, room, entity, aspect, source, point.v),
            )

    def answer(self, query: zenoh.Query) -> None:
        """Answer a home/history/** query.

        Every path keeps at most `limit` rows (issues, for forecasts),
        keeps the newest, and replies oldest first. A limit above
        MAX_QUERY_LIMIT is clamped to it. Malformed parameters get an error
        reply.
        """
        # A query callback that raises sends no reply, and the caller cannot
        # tell that from an answer with no series. A bug in the read path
        # would then look like "nothing was recorded". Any unexpected
        # exception becomes an error reply and a `query-failed` event.
        try:
            self._answer(query)
        except Exception as err:
            self.sess.health_event(
                "query-failed", key=str(query.key_expr), error=str(err)
            )
            query.reply_err(json.dumps(f"query failed: {err}"))

    def _answer(self, query: zenoh.Query) -> None:
        asked = zenoh.KeyExpr(str(query.key_expr))
        # Events only when the selector is inside the events key. The two
        # paths use different from/to formats (integer µs and RFC3339), so
        # one query cannot serve both. A wildcard like home/history/**
        # goes to the sample series.
        if EVENTS_KEY.includes(asked):
            self._answer_events(query)
        elif STATS_KEY.includes(asked):
            self._answer_stats(query)
        elif FORECAST_KEY.includes(asked):
            # Same rule as events: the forecast path takes different
            # parameters and replies with issues instead of rows, so a
            # wildcard like home/history/** answers from the sample series
            # only.
            self._answer_forecasts(query, asked)
        else:
            self._answer_samples(query, asked)

    def _answer_samples(self, query: zenoh.Query, asked: zenoh.KeyExpr) -> None:
        """Answer home/history/{state|cmd}/{entity}/{aspect}?from=..;to=..;limit=..

        from/to are RFC3339 timestamps with a UTC offset. The reply is one
        message per matching series, a JSON array of {ts, room, value}.
        Two optional, mutually exclusive parameters serve charts, and both
        fold the whole window before `limit` keeps the newest rows:
        bucket=<seconds> replies one point per bucket (see `bucketed`), and
        changes=1 replies only the rows where the value changed (see
        `changes_only`).
        """
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
                    # Fold the whole window as it streams from the cursor,
                    # so a wide window does not need memory. A SQL LIMIT
                    # here would cut the window before the fold.
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
        """Answer home/history/forecast/{entity}/{aspect}/{source} in issues.

        `at=<rfc3339>` (default: now) is the forecast as it stood then, the
        latest issue at or before that instant. `valid_from=..;valid_to=..`
        is every issue that said something about that window, each with
        only the points that overlap it. That is what checking a forecast
        against the recorded state for the same span reads. The two are
        exclusive, and the window needs both ends.

        `limit` counts issues, and a reply never splits an issue. Each issue
        comes back in the wire's own shape, so a consumer can pass it to
        the SDK's decoder.
        """
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
                "SELECT id, entity, aspect, source FROM series WHERE class = 'forecast'"
            ).fetchall()
            for series_id, entity, aspect, source in series:
                # The source is a segment of the reply key, so two
                # providers for one aspect come back as two series. A
                # caller that wants all of them puts a wildcard in that
                # slot (docs/design.md#forecasts). migrate_v4 renames empty
                # sources to LEGACY_SOURCE, but an empty one is still
                # skipped here: it is not a valid key expression, and the
                # error would fail the query for every other series too.
                if not source:
                    continue
                series_key = f"home/history/forecast/{entity}/{aspect}/{source}"
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
                    # An interval point covers [valid_ts, valid_end); an
                    # instant covers only itself. They need opposite
                    # comparisons at the window's start: an instant at
                    # `from` is inside the window, an interval ending at
                    # `from` is not. The predicate handles both cases.
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
        """Answer home/history/events?key=..;from=..;to=..;limit=..

        Replies one message, a JSON array of {ts, key, payload} from the
        events table. `key` is a zenoh key expression, wildcards included,
        that filters the recorded event keys; without it every key
        matches. from/to are integer microseconds UTC, the recorder's own
        timestamps, unlike the RFC3339 samples path. A from/to outside
        SQLite's signed 64-bit range gets an error reply.
        """
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
                # applies after the match in Python. A LIKE on the
                # pattern's literal prefix narrows the rows SQLite returns,
                # since the default range is all of history.
                wildcards = [i for i, ch in enumerate(key_pattern) if ch in "*$"]
                prefix = key_pattern[: wildcards[0]] if wildcards else key_pattern
                like = (
                    prefix.replace("\\", "\\\\").replace("%", "\\%").replace("_", "\\_") + "%"
                )
                # Read newest first from the cursor and stop at `limit`
                # matches, so memory stays bounded by the limit.
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
        """Answer home/history/stats with one message describing the store.

        The reply has the file and freelist size, the layout version, each
        series' row count, time bounds (RFC3339, keyed by history key) and
        `rows_per_day`, the events table's count and bounds (integer µs, as
        the events path uses), and the sealed archives. It is what an owner
        looks at before choosing a retention window. The per-series figures
        are kept up to date on insert, so the reply reads one row per series
        and does not slow down as the store grows.
        """
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
    """Return a series' long-run write rate in rows per day, or None.

    None for a series too short to have one (a single row, or every row
    inside one microsecond). The reply already carries the three inputs.
    The rate is included because stats is mostly asked which series is
    filling the file, and the owner should not have to compute it for
    hundreds of series.
    """
    span = newest - oldest
    if span <= 0:
        return None
    return round(rows * 86_400 * 1_000_000 / span, 1)


def store_stats(conn: sqlite3.Connection) -> dict:
    """Return what is in the store, for the stats reply.

    Sizes from the pager, one aggregate per series and one for the events
    table. The per-series aggregates are read off `series`, where the
    insert trigger and _purge maintain them; a series with no rows right
    now (`row_count` 0) has no entry.
    """
    page_size = conn.execute("PRAGMA page_size").fetchone()[0]
    page_count = conn.execute("PRAGMA page_count").fetchone()[0]
    freelist = conn.execute("PRAGMA freelist_count").fetchone()[0]
    series = {}
    for space, entity, aspect, source, rows, oldest, newest in conn.execute(
        "SELECT class, entity, aspect, source, row_count, oldest_ts, newest_ts FROM series"
        " WHERE row_count > 0 ORDER BY class, entity, aspect, source"
    ):
        # A forecast series is one source's, keyed as its read path is.
        key = f"home/history/{space}/{entity}/{aspect}"
        if source:
            key = f"{key}/{source}"
        series[key] = {
            "rows": rows,
            "oldest": iso_utc(oldest),
            "newest": iso_utc(newest),
            "rows_per_day": rows_per_day(rows, oldest, newest),
        }
    rows, oldest, newest = conn.execute("SELECT COUNT(*), MIN(ts), MAX(ts) FROM events").fetchone()
    # The sealed months, read from the `archives` record: one row per
    # file, without opening the files.
    archives = [
        {
            "file": name,
            "month": month,
            "samples": samples,
            "forecasts": forecasts,
            "events": events,
            "bytes": size,
            "sha256": digest,
            "sealed": iso_utc(sealed),
        }
        for name, month, samples, forecasts, events, size, digest, sealed in conn.execute(
            "SELECT file, month, samples, forecasts, events, bytes, sha256, sealed_ts"
            " FROM archives WHERE state = 'sealed' ORDER BY month, sealed_ts"
        )
    ]
    return {
        "store_version": conn.execute("PRAGMA user_version").fetchone()[0],
        "file_bytes": page_count * page_size,
        "freelist_bytes": freelist * page_size,
        "series": series,
        "events": {"rows": rows, "oldest": oldest, "newest": newest},
        "archives": archives,
    }


def event_payload(text: str):
    """Return an event's payload parsed as JSON, or as its raw string.

    Events are recorded raw, and any bus client can put on these keys, so
    a row may not be JSON. It is returned as a string so one such row does
    not break every events query that includes it.
    """
    try:
        return json.loads(text)
    except ValueError:
        return text


def split_selector(raw: str) -> dict[str, str]:
    """Split a selector's parameters (zenoh's `a=1;b=2` grammar) into a dict.

    No URL decoding: RFC3339 offsets contain '+', which must stay literal.
    """
    params = {}
    for part in raw.split(";"):
        if not part:
            continue
        name, _, value = part.partition("=")
        params[name] = value
    return params


def parse_params(raw: str) -> tuple[int, int, int, int, bool]:
    """Parse a samples query's parameters from a selector.

    Returns from/to (RFC3339 with offset), limit, bucket (µs, 0 = raw rows)
    and changes.
    """
    params = split_selector(raw)
    from_us, to_us, limit = 0, now_us(), DEFAULT_QUERY_LIMIT
    bucket_us, changes = 0, False
    for bound in ("from", "to"):
        if bound not in params:
            continue
        try:
            dt = datetime.datetime.fromisoformat(params[bound])
        except ValueError:
            raise ValueError(f"{bound}: {params[bound]!r} is not RFC3339") from None
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
        # The fold walks the whole window, so a bucket that yields more
        # points than a reply can carry would scan for nothing.
        raise ValueError(
            f"bucket: {bucket_us // 1_000_000} s makes more than {MAX_QUERY_LIMIT}"
            " buckets over the window; widen it or narrow from/to"
        )
    return from_us, to_us, limit, bucket_us, changes


def as_issues(rows, limit: int) -> list:
    """Group rows into issues, in the wire's shape.

    When there are more than `limit` issues the newest are kept. The
    samples path keeps the newest rows; this keeps whole issues, because
    part of an issue is not a forecast.
    """
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
    """Parse a forecast query's parameters from a selector.

    `at` or `valid_from`+`valid_to` (RFC3339 with offset, as the samples
    path spells time), plus `limit` in issues.

    The two are exclusive and the range needs both ends. An open window
    over a store of superseded issues would scan the whole table, and a
    default for one end would be a guess. With neither given, `at` is
    now, so a bare read of the key returns the current forecast.
    """
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
    """Parse an RFC3339 instant with an offset, returned in µs.

    The samples path's convention, shared so the forecast path parses
    time the same way.
    """
    try:
        dt = datetime.datetime.fromisoformat(raw)
    except ValueError:
        raise ValueError(f"{name}: {raw!r} is not RFC3339") from None
    if dt.tzinfo is None:
        raise ValueError(f"{name}: {raw!r} needs a UTC offset")
    return int(dt.timestamp() * 1e6)


def parse_bucket(raw: str) -> int:
    """Parse a positive bucket width in whole seconds, returned in µs."""
    try:
        seconds = int(raw)
    except ValueError:
        raise ValueError(f"bucket: {raw!r} is not an integer") from None
    if seconds < 1:
        raise ValueError(f"bucket: {seconds} is not positive")
    if seconds > INT64_MAX // 1_000_000:
        raise ValueError(f"bucket: {seconds} is out of range")
    return seconds * 1_000_000


def bucketed(rows, bucket_us: int, limit: int) -> list[dict]:
    """Fold ascending (ts, room, kind, value) rows into one point per bucket.

    A cursor, folded as it streams, the newest `limit` kept: ts is the
    bucket's start, room the last row's; a number bucket's value is the
    mean and carries min and max, any other kind's is the last value seen
    (a run of bools or enum strings has no mean).
    """
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
    """Return the rows at which the value changed, the window's first included.

    The newest `limit` kept: a state's runs, for timelines. Repeats are
    kept in the store (each is a sighting) and collapsed here, on read, as
    the cursor streams.
    """
    out: deque[tuple] = deque(maxlen=limit)
    last = None
    for row in rows:
        if last is None or last[2:] != row[2:]:
            out.append(row)
        last = row
    return list(out)


def parse_limit(raw: str) -> int:
    """Parse a positive row count, clamped to MAX_QUERY_LIMIT."""
    try:
        limit = int(raw)
    except ValueError:
        raise ValueError(f"limit: {raw!r} is not an integer") from None
    if limit < 1:
        raise ValueError(f"limit: {limit} is not positive")
    return min(limit, MAX_QUERY_LIMIT)


def parse_event_params(raw: str) -> tuple[str | None, int, int, int]:
    """Parse an events query's parameters from a selector.

    Returns key (a zenoh key expression filtering recorded event keys,
    wildcards included; None means all), from/to (integer microseconds UTC,
    the recorder's own timestamps, unlike the RFC3339 samples path) and
    limit.
    """
    params = split_selector(raw)
    key = params.get("key")
    if key is not None:
        try:
            zenoh.KeyExpr(key)
        except zenoh.ZError as err:
            raise ValueError(f"key: {key!r} is not a valid key expression: {err}") from err
    from_us, to_us, limit = 0, now_us(), DEFAULT_EVENTS_LIMIT
    for bound in ("from", "to"):
        if bound not in params:
            continue
        try:
            us = int(params[bound])
        except ValueError:
            raise ValueError(f"{bound}: {params[bound]!r} is not an integer") from None
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
    """Create the store and its schema, or migrate an older layout in place.

    Runs before ready(), so a recorder without a working store does not
    report ready.
    """
    db_path.parent.mkdir(parents=True, exist_ok=True)
    conn = sqlite3.connect(db_path)
    try:
        # Both pragmas persist in the file. Incremental auto_vacuum lets a
        # retention delete return pages to the filesystem. It only takes
        # effect before the file's first page is written (or across a
        # VACUUM), so it comes first. WAL keeps the per-flush writer and
        # the read-only query connections from blocking each other.
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
        if existing and version < 4:
            migrate_v3(conn)
        if existing and version < 5:
            migrate_v4(conn)
        conn.execute(f"PRAGMA user_version={STORE_VERSION}")
        conn.commit()
    finally:
        conn.close()


def has_samples_table(conn: sqlite3.Connection) -> bool:
    """Return whether this file already holds a store.

    A fresh one needs no migration, and its `series` is empty either way.
    """
    return bool(list(conn.execute("PRAGMA table_info(samples)")))


def has_v0_samples(conn: sqlite3.Connection) -> bool:
    columns = {row[1] for row in conn.execute("PRAGMA table_info(samples)")}
    return "class" in columns


def migrate_v0(conn: sqlite3.Connection) -> None:
    """Migrate version 0 -> 1: the wide samples table becomes series + rooms + narrow samples.

    One explicit transaction, so a crash mid-way leaves the version-0 file
    intact for the next start; the VACUUM after it is what switches an
    existing file to auto_vacuum.
    """
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
    """Migrate version 1 -> 2: the per-series tally arrives.

    An existing store's tally is counted once, at startup, where nothing
    waits on a query timeout. Each aggregate is a correlated subquery on
    the clustered primary key instead of one GROUP BY over the table: the
    counts walk each series' key range, the bounds are seeks to its ends,
    and it needs no newer SQLite than the schema does. On a synthetic 4.8 M-row store this took 0.13 s, against 0.62 s
    for the GROUP BY. A store migrating straight from version 0 already
    has the columns, because SCHEMA built its new tables, and only needs
    the count.
    """
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


def migrate_v3(conn: sqlite3.Connection) -> None:
    """Migrate version 3 -> 4: add `source` to `series` and to its uniqueness.

    Uniqueness moves from (class, entity, aspect) to (class, entity,
    aspect, source), so two providers forecasting one aspect are two
    series (docs/design.md#history-and-the-recorder).

    A column can be added in place, but the old uniqueness cannot be
    removed in place. It is a table-level UNIQUE, which SQLite implements
    as an auto-index that `DROP INDEX` refuses. So the table is rebuilt
    the way SQLite documents: create, copy, drop, rename. That is safe
    here because foreign keys are not enforced and the rename restores
    the name the other tables reference.

    Existing forecast rows keep source ''. The store did not record who
    issued them, and a made-up provider name would be false provenance.
    migrate_v4 then renames them to LEGACY_SOURCE, because an empty source
    cannot be queried. A producer republishing under a real source starts
    a new series beside them.
    """
    columns = {row[1] for row in conn.execute("PRAGMA table_info(series)")}
    if "source" in columns:
        return
    # migrate_v1 may have just run and left its UPDATE in an implicit
    # transaction. The BEGIN below would then raise "cannot start a
    # transaction within a transaction", and the recorder would fail to
    # open its store.
    conn.commit()
    # The triggers UPDATE `series` by name, and SQLite 3.25 and later
    # revalidate every trigger body during ALTER TABLE RENAME. With
    # `series` dropped, the rename fails with "error in trigger
    # samples_tally: no such table: main.series". legacy_alter_table skips
    # that revalidation. The triggers are valid again once the rebuilt
    # table has the name back.
    conn.execute("PRAGMA legacy_alter_table=ON")
    conn.execute("BEGIN")
    # Mirrors the `series` definition in SCHEMA; a test pins the columns
    # so the two cannot drift apart.
    conn.execute(
        "CREATE TABLE series_v4 ("
        "  id INTEGER PRIMARY KEY,"
        "  class TEXT NOT NULL,"
        "  entity TEXT NOT NULL,"
        "  aspect TEXT NOT NULL,"
        "  source TEXT NOT NULL DEFAULT '',"
        "  row_count INTEGER NOT NULL DEFAULT 0,"
        "  oldest_ts INTEGER,"
        "  newest_ts INTEGER,"
        "  UNIQUE (class, entity, aspect, source)"
        ")"
    )
    conn.execute(
        "INSERT INTO series_v4 (id, class, entity, aspect, source,"
        "                       row_count, oldest_ts, newest_ts)"
        " SELECT id, class, entity, aspect, '', row_count, oldest_ts, newest_ts"
        " FROM series"
    )
    conn.execute("DROP TABLE series")
    conn.execute("ALTER TABLE series_v4 RENAME TO series")
    conn.commit()
    conn.execute("PRAGMA legacy_alter_table=OFF")


def migrate_v4(conn: sqlite3.Connection) -> None:
    """Migrate version 4 -> 5: give empty-source forecast series the reserved name.

    The source is a segment of the reply key. An empty segment is a key
    expression SQLite stores and zenoh refuses to parse, so the read path
    skips a series with an empty source and its rows cannot be read.
    Under LEGACY_SOURCE they can.

    Only forecast series are changed. Every other class has an empty
    source by definition and does not put it in a key.
    """
    # OR IGNORE because a store could already hold the series under both
    # names. It then keeps the empty one, which the read path skips,
    # instead of failing the migration and the recorder's startup.
    conn.execute(
        "UPDATE OR IGNORE series SET source = ?"
        " WHERE class = 'forecast' AND source = ''",
        (LEGACY_SOURCE,),
    )
    conn.commit()


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
