# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "homeostat",
# ]
#
# [tool.uv.sources]
# homeostat = { path = "../../../../sdk/python", editable = true }
# ///
"""Report what `subscribe` delivered for the lamp, with its age.

The fixture for replay after a core restart
(docs/design.md#replay-after-a-core-restart). Every half second it
publishes the last value and age it was given as a `seen` health event,
so a test that subscribes late still hears it.
"""

import threading
import time

from homeostat import automation


def main():
    ctx = automation.context()
    seen = {}
    lock = threading.Lock()

    def on_lamp(key, value, age_s):
        with lock:
            seen.update(value=value, age_s=age_s)

    ctx.subscribe("lamp", on_lamp)
    ctx.ready()

    def report():
        while True:
            time.sleep(0.5)
            with lock:
                last = dict(seen)
            if last:
                ctx.health_event("seen", **last)

    threading.Thread(target=report, daemon=True).start()
    ctx.run()


if __name__ == "__main__":
    main()
