# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "homeostat",
# ]
#
# [tool.uv.sources]
# homeostat = { path = "../../../sdk/python", editable = true }
# ///
"""Fused temperature: the first virtual sensor (docs/design.md, Virtual
sensors).

Publishes the mean of its fresh source temperatures onto the entity it
binds, on transition only — an input update that does not move the mean
publishes nothing. A source silent for longer than `source_max_age_s`
leaves the mean until it publishes again: `available` is device liveness,
not data freshness, so the staleness policy is the automation's own.
"""

import threading

from homeostat import Freshness, automation


def main():
    ctx = automation.context()
    lock = threading.Lock()
    sources = Freshness()
    last: float | None = None

    def on_temperature(key, value):
        nonlocal last
        if isinstance(value, bool) or not isinstance(value, (int, float)):
            return
        with lock:
            sources.seen(key, float(value))
            fresh = sources.fresh(ctx.params.source_max_age_s)
            fused = round(sum(fresh.values()) / len(fresh), 2)
            if fused == last:
                return
            last = fused
        ctx.publish("fused", fused)

    ctx.subscribe("livingroom", on_temperature)
    ctx.subscribe("office", on_temperature)
    ctx.ready()
    ctx.run()


if __name__ == "__main__":
    main()
