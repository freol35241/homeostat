# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "homeostat",
# ]
#
# [tool.uv.sources]
# homeostat = { path = "../../../../sdk/python", editable = true }
# ///
"""A rain gauge that reads its own readings back from the recorder.

This is the fixture for `ctx.history`, stamped publishes,
`ctx.history_total` and `ctx.forecast_history`. It publishes three hourly
amounts, each stamped with the start of its hour, waits until the
recorder holds them, and then publishes what it read:

- `rows`, the raw row count;
- `exact`, whether each row is stored at its hour's start to the
  microsecond;
- `total` and `count`, the buckets' sums and sample counts added up;
- `window`, the total from half past the first hour to the end of the
  last, so the first hour counts half;
- `short`, the reason a window running past the last hour is refused.

It then publishes two issues of a forecast for the last hour, an hour
apart, and publishes what it read back:

- `issues`, how many issues covered that hour;
- `as_of`, the value the forecast gave as it stood between the issues;
- `current`, the value the forecast gives now;
- `newest`, whether `limit=1` keeps the later issue.

The test compares them with what was published.
"""

import datetime
import time

from homeostat import IntervalsUncovered, automation

AMOUNTS = [0.5, 0.0, 1.5]


def main():
    ctx = automation.context()
    now = datetime.datetime.now(datetime.timezone.utc)
    hour = now.replace(minute=0, second=0, microsecond=0)
    hours = [hour - datetime.timedelta(hours=len(AMOUNTS) - i) for i in range(len(AMOUNTS))]
    start = hours[0]
    # The first read waits for the recorder, which subscribes before it
    # answers. Published any earlier, the amounts could go unrecorded.
    ctx.history("rain", start, start)
    for at, amount in zip(hours, AMOUNTS, strict=True):
        ctx.publish("gauge", amount, aspect="precipitation", at=at)
    ctx.ready()
    deadline = time.monotonic() + 60
    while True:
        end = datetime.datetime.now(datetime.timezone.utc)
        rows = ctx.history("rain", start, end)
        if len(rows) == len(AMOUNTS) or time.monotonic() > deadline:
            break
        time.sleep(0.2)
    buckets = ctx.history("rain", start, end, bucket_s=86400)
    ctx.publish("gauge", len(rows), aspect="rows")
    ctx.publish("gauge", [row["ts"] for row in rows] == hours, aspect="exact")
    ctx.publish("gauge", sum(b["sum"] for b in buckets), aspect="total")
    ctx.publish("gauge", sum(b["n"] for b in buckets), aspect="count")
    half_past = hours[0] + datetime.timedelta(minutes=30)
    last_end = hours[-1] + datetime.timedelta(hours=1)
    ctx.publish("gauge", ctx.history_total("rain", half_past, last_end), aspect="window")
    try:
        ctx.history_total("rain", half_past, last_end + datetime.timedelta(hours=1))
    except IntervalsUncovered as err:
        ctx.publish("gauge", err.reason, aspect="short")

    first, second = hours[0], hours[1]
    ctx.publish_forecast("outlook", first, [(hours[2], 1.0, 3600)])
    ctx.publish_forecast("outlook", second, [(hours[2], 2.0, 3600)])
    deadline = time.monotonic() + 60
    while True:
        covering = ctx.forecast_history("outlook", valid_from=hours[2], valid_to=last_end)
        if len(covering) == 2 or time.monotonic() > deadline:
            break
        time.sleep(0.2)
    ctx.publish("gauge", len(covering), aspect="issues")
    as_of = ctx.forecast_history("outlook", at=first + datetime.timedelta(minutes=30))
    ctx.publish("gauge", as_of[0].points[0].v, aspect="as_of")
    ctx.publish("gauge", ctx.forecast_history("outlook")[0].points[0].v, aspect="current")
    newest = ctx.forecast_history("outlook", valid_from=hours[2], valid_to=last_end, limit=1)
    ctx.publish("gauge", [issue.issued for issue in newest] == [second], aspect="newest")
    ctx.run()


if __name__ == "__main__":
    main()
