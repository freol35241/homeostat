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

This is the fixture for `ctx.history`. It publishes three hourly amounts,
waits until the recorder holds them, and then publishes what a raw read
and a bucketed read returned: `rows`, the raw row count, and `total` and
`count`, the buckets' sums and sample counts added up. The test compares
them with what was published.
"""

import datetime
import time

from homeostat import automation

AMOUNTS = [0.5, 0.0, 1.5]


def main():
    ctx = automation.context()
    start = datetime.datetime.now(datetime.timezone.utc)
    # The first read waits for the recorder, which subscribes before it
    # answers. Published any earlier, the amounts could go unrecorded.
    ctx.history("rain", start, start)
    for amount in AMOUNTS:
        ctx.publish("gauge", amount, aspect="precipitation")
        time.sleep(0.05)
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
    ctx.publish("gauge", sum(b["sum"] for b in buckets), aspect="total")
    ctx.publish("gauge", sum(b["n"] for b in buckets), aspect="count")
    ctx.run()


if __name__ == "__main__":
    main()
