# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "homeostat",
# ]
#
# [tool.uv.sources]
# homeostat = { path = "../../../sdk/python", editable = true }
# ///
"""Evening lights: the first automation (see docs/design.md).

A regulator, not a scheduler: on every input — clock minute, presence
change, light state — it re-evaluates one rule: inside the night window
(off_time until 05:00, window may span midnight) with nobody present,
every light that is on gets turned off. Lights are known from their state
keys; commands go back through the manifest's publish expression. off_time
is a live parameter: an edit applies at the next evaluation, no restart.
"""

import datetime
import threading

from homeostat import automation

NIGHT_END = datetime.time(5, 0)


def in_night_window(t: datetime.time, off: datetime.time) -> bool:
    if off <= NIGHT_END:  # off_time itself is past midnight
        return off <= t < NIGHT_END
    return t >= off or t < NIGHT_END


def main():
    ctx = automation.context()
    lock = threading.Lock()
    present = False
    minute: datetime.time | None = None
    lights_on: dict[tuple[str, str], bool] = {}

    def evaluate():
        with lock:
            if minute is None or present:
                return
            if not in_night_window(minute, ctx.params.off_time):
                return
            targets = [key for key, on in lights_on.items() if on]
        for room, entity in targets:
            ctx.publish("lights", False, room=room, entity=entity)

    def on_clock(key, value):
        nonlocal minute
        with lock:
            minute = datetime.datetime.fromisoformat(value).time()
        evaluate()

    def on_presence(key, value):
        nonlocal present
        with lock:
            present = value is True
        evaluate()

    def on_light_state(key, value):
        room, entity = key.split("/")[2:4]
        with lock:
            lights_on[(room, entity)] = value is True
        evaluate()

    ctx.subscribe("clock", on_clock)
    ctx.subscribe("presence", on_presence)
    ctx.subscribe("light_state", on_light_state)
    ctx.ready()
    ctx.run()


if __name__ == "__main__":
    main()
