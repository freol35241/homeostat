# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "homeostat",
# ]
#
# [tool.uv.sources]
# homeostat = { path = "../sdk/python", editable = true }
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
replied oldest-to-newest.
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

BUFFER_LIMIT = 10_000
RETRY_S = 1.0
DEFAULT_QUERY_LIMIT = 1000
DEFAULT_EVENTS_LIMIT = 500
EVENTS_KEY = zenoh.KeyExpr("home/history/events")

SCHEMA = """
CREATE TABLE IF NOT EXISTS samples (
  ts INTEGER NOT NULL,
  class TEXT NOT NULL,
  room TEXT NOT NULL,
  entity TEXT NOT NULL,
  aspect TEXT NOT NULL,
  kind TEXT NOT NULL,
  value NOT NULL
);
CREATE INDEX IF NOT EXISTS samples_series ON samples (class, entity, aspect, ts);
CREATE TABLE IF NOT EXISTS events (
  ts INTEGER NOT NULL,
  key TEXT NOT NULL,
  payload TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS events_key ON events (key, ts);
"""


def now_us() -> int:
    return time.time_ns() // 1_000


def iso_utc(us: int) -> str:
    return datetime.datetime.fromtimestamp(us / 1e6, tz=datetime.timezone.utc).isoformat(
        timespec="microseconds"
    )


class Writer:
    """Single writer thread draining a bounded queue, one transaction per
    flush. Failed batches stay pending and retry on new samples or a timer."""

    def __init__(self, db_path: Path, sess: session.UnitSession):
        self.db_path = db_path
        self.sess = sess
        self.cond = threading.Condition()
        self.queue: deque = deque()
        self.stopping = False
        self.thread = threading.Thread(target=self._run, name="writer")

    def enqueue(self, table: str, row: tuple) -> None:
        with self.cond:
            self.queue.append((table, row))
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
        while True:
            with self.cond:
                while not self.queue and not pending and not self.stopping:
                    self.cond.wait()
                if not self.queue and not pending and self.stopping:
                    return
                pending.extend(self.queue)
                self.queue.clear()
                stopping = self.stopping
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
                conn.executemany(
                    "INSERT INTO samples VALUES (?, ?, ?, ?, ?, ?, ?)",
                    [row for table, row in rows if table == "samples"],
                )
                conn.executemany(
                    "INSERT INTO events VALUES (?, ?, ?)",
                    [row for table, row in rows if table == "events"],
                )
        finally:
            conn.close()


def typed(value):
    """(kind, stored value) for a scalar JSON value, None for non-scalars."""
    if isinstance(value, bool):
        return "bool", int(value)
    if isinstance(value, (int, float)):
        return "number", value
    if isinstance(value, str):
        return "string", value
    return None


def decode(kind: str, value):
    return bool(value) if kind == "bool" else value


class Recorder:
    def __init__(self, db_path: Path, sess: session.UnitSession):
        self.db_path = db_path
        self.sess = sess
        self.writer = Writer(db_path, sess)

    def record(self, sample: zenoh.Sample) -> None:
        ts = now_us()
        key = str(sample.key_expr)
        parts = key.split("/")
        if len(parts) > 1 and parts[1] in ("state", "cmd"):
            self._record_sample(ts, key, parts, sample)
        else:
            payload = sample.payload.to_bytes().decode("utf-8", errors="replace")
            self.writer.enqueue("events", (ts, key, payload))

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
        row = (ts, parts[1], parts[2], parts[3], "/".join(parts[4:]), kind, stored)
        self.writer.enqueue("samples", row)
        if parts[1] == "cmd":
            # The "who" audit design.md anticipated: the full envelope
            # (value, priority, actor) lands in events alongside the
            # unwrapped value in samples.
            raw = sample.payload.to_bytes().decode("utf-8", errors="replace")
            self.writer.enqueue("events", (ts, key, raw))

    def answer(self, query: zenoh.Query) -> None:
        asked = zenoh.KeyExpr(str(query.key_expr))
        if asked.intersects(EVENTS_KEY):
            self._answer_events(query)
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
                "SELECT DISTINCT class, entity, aspect FROM samples"
            ).fetchall()
            for space, entity, aspect in series:
                series_key = f"home/history/{space}/{entity}/{aspect}"
                if not asked.intersects(zenoh.KeyExpr(series_key)):
                    continue
                rows = conn.execute(
                    "SELECT ts, room, kind, value FROM ("
                    "  SELECT ts, room, kind, value FROM samples"
                    "  WHERE class = ? AND entity = ? AND aspect = ?"
                    "    AND ts >= ? AND ts <= ?"
                    "  ORDER BY ts DESC LIMIT ?"
                    ") ORDER BY ts ASC",
                    (space, entity, aspect, from_us, to_us, limit),
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
            rows = conn.execute(
                "SELECT ts, key, payload FROM events WHERE ts >= ? AND ts <= ?"
                " ORDER BY ts DESC",
                (from_us, to_us),
            ).fetchall()
            if key_pattern is not None:
                pattern = zenoh.KeyExpr(key_pattern)
                rows = [row for row in rows if pattern.intersects(zenoh.KeyExpr(row[1]))]
            payload = [
                {"ts": ts, "key": key, "payload": json.loads(text)}
                for ts, key, text in reversed(rows[:limit])
            ]
            query.reply("home/history/events", json.dumps(payload))
        except sqlite3.Error as err:
            query.reply_err(json.dumps(f"store unavailable: {err}"))
        finally:
            conn.close()


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
    """Creates the store and its schema. Must succeed before ready(): a
    recorder that never had a working store must not claim readiness."""
    db_path.parent.mkdir(parents=True, exist_ok=True)
    conn = sqlite3.connect(db_path)
    try:
        conn.executescript(SCHEMA)
        conn.commit()
    finally:
        conn.close()


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
    sess.ready()

    stop = threading.Event()
    signal.signal(signal.SIGTERM, lambda *_: stop.set())
    signal.signal(signal.SIGINT, lambda *_: stop.set())
    stop.wait()

    for sub in subs:
        sub.undeclare()
    queryable.undeclare()
    recorder.writer.stop()
    sess.close()


if __name__ == "__main__":
    main()
