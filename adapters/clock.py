# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "homeostat",
# ]
#
# [tool.uv.sources]
# homeostat = { path = "../sdk/python", editable = true }
# ///
"""Clock service: civil time on the bus (see docs/design.md#unit-kinds).

Publishes home/clock/minute (RFC3339 local time with offset, on the
minute) and home/clock/date (at local midnight). Both are also published
at startup, so a restarted subscriber does not wait up to 59 seconds for
the time.

The service owns the timezone and DST, so subscribers do no naive time
arithmetic. The timezone is the `timezone` manifest parameter and follows
live edits. An invalid live value leaves a `drop` health event (reason
invalid-timezone) and keeps the last good zone. An invalid value at
startup is a startup error, visible through the supervisor's backoff.
"""

import datetime
import signal
import threading
from zoneinfo import ZoneInfo, ZoneInfoNotFoundError

from homeostat import automation


def next_boundary(now_utc: datetime.datetime) -> datetime.datetime:
    """Return the next whole UTC minute after `now_utc`.

    Computed in UTC, not the local wall clock, so a DST transition cannot
    make one wait longer or shorter than a real minute. At fall-back, the
    local clock's `replace(...) + timedelta(minutes=1)` names a wall-clock
    minute 61 real minutes away, skipping the repeated hour. UTC has no
    such transitions, so the wait is one real minute, and the local zone,
    applied only when publishing, shows the repeated hour each time it
    occurs.
    """
    return now_utc.replace(second=0, microsecond=0) + datetime.timedelta(minutes=1)


def main():
    ctx = automation.context()
    zone = ZoneInfo(ctx.params.timezone)

    stop = threading.Event()
    signal.signal(signal.SIGTERM, lambda *_: stop.set())
    signal.signal(signal.SIGINT, lambda *_: stop.set())

    last_date = None

    def publish(now):
        nonlocal last_date
        minute = now.replace(second=0, microsecond=0)
        ctx.publish("minute", minute.isoformat())
        if minute.date() != last_date:
            last_date = minute.date()
            ctx.publish("date", last_date.isoformat())

    publish(datetime.datetime.now(zone))
    ctx.ready()

    while True:
        now_utc = datetime.datetime.now(datetime.timezone.utc)
        boundary_utc = next_boundary(now_utc)
        if stop.wait(timeout=(boundary_utc - now_utc).total_seconds()):
            break
        if datetime.datetime.now(datetime.timezone.utc) < boundary_utc:
            # Event.wait measures monotonic time, so after NTP slews the
            # clock back this would republish the previous minute and fire
            # minute-tick automations twice. Wait out the remainder. The
            # check is in UTC, so a DST fall-back's repeated hour is not
            # mistaken for a slew (see next_boundary).
            continue
        try:
            zone = ZoneInfo(ctx.params.timezone)
        except (ZoneInfoNotFoundError, TypeError, ValueError):
            # A live, editable string: an unknown key, a malformed one, or
            # a non-string value from a raw bus write.
            ctx.health_event("drop", reason="invalid-timezone", value=ctx.params.timezone)
        publish(datetime.datetime.now(zone))

    ctx.close()


if __name__ == "__main__":
    main()
