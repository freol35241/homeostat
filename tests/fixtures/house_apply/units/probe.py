# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "eclipse-zenoh",
# ]
# ///
"""Probe automation for the plan/apply fixture.

Echoes its live `level` parameter to `home/state/den/probe_echo/level` on
every config update, so a test observes a parameter change reaching a
running unit with no restart. Deliberately SDK-free (zenoh only): the
fixture is copied into temp dirs where a path-sourced SDK would not
resolve.
"""
# MARKER v1

import json
import os
import signal
import threading

import zenoh

UNIT = os.environ["HOMEOSTAT_UNIT"]
BUS = os.environ["HOMEOSTAT_BUS"]
CONFIG_KEY = f"home/config/{UNIT}/level"
ECHO_KEY = "home/state/den/probe_echo/level"


def main():
    conf = zenoh.Config()
    conf.insert_json5("mode", '"client"')
    conf.insert_json5("connect/endpoints", json.dumps([BUS]))
    conf.insert_json5("scouting/multicast/enabled", "false")
    conf.insert_json5("scouting/gossip/enabled", "false")
    session = zenoh.open(conf)

    def echo(payload: bytes) -> None:
        session.put(ECHO_KEY, payload)

    # Subscribe, then get, merge: the get covers everything before the
    # subscription, the subscriber everything after.
    sub = session.declare_subscriber(
        CONFIG_KEY, lambda sample: echo(sample.payload.to_bytes())
    )
    for reply in session.get(CONFIG_KEY):
        if reply.ok is not None:
            echo(reply.ok.payload.to_bytes())

    token = session.liveliness().declare_token(f"home/health/{UNIT}/alive")

    stop = threading.Event()
    signal.signal(signal.SIGTERM, lambda *_: stop.set())
    signal.signal(signal.SIGINT, lambda *_: stop.set())
    stop.wait()

    token.undeclare()
    sub.undeclare()
    session.close()


main()
