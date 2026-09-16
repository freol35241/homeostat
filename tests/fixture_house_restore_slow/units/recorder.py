# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "homeostat",
# ]
#
# [tool.uv.sources]
# homeostat = { path = "../../../sdk/python", editable = true }
# ///
"""A recorder that is up but not answering yet (#102).

Stands in for adapters/recorder.py in the one respect `ctx.restore` waits
on: its history queryable exists, but every query is held for SLOW_S from
startup before it is answered — longer than zenoh's default get timeout,
so a client that reads one timed-out get as "no" gives up, while one that
reads it as "not yet" is answered on a later poll. After the window it
answers `stats` and the latch's series from one canned row, the value a
person once decided and the only record of it.
"""

import json
import signal
import threading
import time

from homeostat import session

SLOW_S = 12.0
DECIDED = True


def main():
    sess = session.connect()
    answering_at = time.monotonic() + SLOW_S

    def answer(query):
        time.sleep(max(0.0, answering_at - time.monotonic()))
        key = str(query.key_expr)
        if key.endswith("/stats"):
            query.reply(key, json.dumps({"store_version": 1}))
        else:
            query.reply(
                key, json.dumps([{"ts": "2026-01-01T00:00:00+00:00", "value": DECIDED}])
            )

    queryable = sess.declare_queryable("home/history/**", answer)
    sess.ready()

    stop = threading.Event()
    signal.signal(signal.SIGTERM, lambda *_: stop.set())
    signal.signal(signal.SIGINT, lambda *_: stop.set())
    stop.wait()
    queryable.undeclare()
    sess.close()


if __name__ == "__main__":
    main()
