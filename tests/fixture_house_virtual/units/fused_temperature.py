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

Publishes the mean of its source temperatures onto the entity it binds,
on transition only — an input update that does not move the mean
publishes nothing.
"""

import threading

from homeostat import automation


def main():
    ctx = automation.context()
    lock = threading.Lock()
    sources: dict[str, float] = {}
    last: float | None = None

    def on_temperature(key, value):
        nonlocal last
        if isinstance(value, bool) or not isinstance(value, (int, float)):
            return
        with lock:
            sources[key] = float(value)
            fused = round(sum(sources.values()) / len(sources), 2)
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
