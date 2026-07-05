# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "homeostat",
# ]
#
# [tool.uv.sources]
# homeostat = { git = "https://github.com/freol35241/homeostat", subdirectory = "sdk/python", tag = "v0.1.0" }
# ///
"""Clock service: civil time on the bus (see docs/design.md).

Publishes home/clock/minute (RFC3339 local time with offset, on the minute)
and home/clock/date (at local midnight). The current minute and date are
published immediately at startup — late-joiner catch-up, so a restarted
subscriber never runs blind for up to 59 seconds.

The service owns timezone and DST: subscribers never do naive time
arithmetic. The timezone is the `timezone` manifest parameter and follows
live edits; an invalid live value leaves a health event and the last good
zone in effect (an invalid value at startup is a startup error, made
visible by the supervisor's backoff).
"""

import datetime
import signal
import threading
from zoneinfo import ZoneInfo

from homeostat import automation


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
        now = datetime.datetime.now(zone)
        boundary = now.replace(second=0, microsecond=0) + datetime.timedelta(minutes=1)
        if stop.wait(timeout=(boundary - now).total_seconds()):
            break
        try:
            zone = ZoneInfo(ctx.params.timezone)
        except Exception:
            ctx.health_event("drop", reason="invalid-timezone", value=ctx.params.timezone)
        publish(datetime.datetime.now(zone))

    ctx.close()


if __name__ == "__main__":
    main()
