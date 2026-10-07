# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "homeostat",
# ]
#
# [tool.uv.sources]
# homeostat = { path = "../../../sdk/python", editable = true }
# ///
"""A recorder that is up but slow to answer (#102, #121).

Stands in for adapters/recorder.py in the one respect `ctx.restore` waits
on: its history queryable exists, but every `stats` answer takes SLOW_S,
and like the real recorder it answers one query at a time (zenoh runs a
queryable's callback serially). SLOW_S is longer than zenoh's default get
timeout, so a client that reads one timed-out get as "no" gives up (#102);
and a client that gives up on a short get and asks again only queues
another slow answer behind the one it abandoned, so it never hears a
reply at all (#121). A client that asks once and waits is answered. The
latch's series is answered at once from one canned row, the value a
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

    def answer(query):
        key = str(query.key_expr)
        if key.endswith("/stats"):
            time.sleep(SLOW_S)
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
