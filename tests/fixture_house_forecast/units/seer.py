# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "homeostat",
# ]
#
# [tool.uv.sources]
# homeostat = { path = "../../../sdk/python", editable = true }
# ///
"""A forecast producer, standing in for a house's own (docs/design.md,
Forecasts).

Deliberately not an adapter for any real source: which prices, and what to
do about them, are house content by the boundary test (docs/design.md,
Repo split). This exists so the bus class, the SDK codec and the core's
mirror are exercised end to end by something shaped like the real thing —
an irregular horizon, published once, with the current value alongside it
on the state class.
"""

import datetime
import time

import homeostat
from homeostat import keys

ROOM = "global"
ENTITY = "spot_price"
ASPECT = "price"

# Hourly, then a three-hour step: the irregular shape real sources publish,
# and the one a regular grid could not have carried.
OFFSETS_H = [0, 1, 2, 3, 6, 9]
VALUES = [1.20, 1.45, 1.10, 0.85, 0.40, 0.95]


def main() -> None:
    session = homeostat.connect()
    try:
        issued = datetime.datetime.now(datetime.timezone.utc).replace(
            minute=0, second=0, microsecond=0
        )
        session.put_json(keys.state_key(ROOM, ENTITY, ASPECT), VALUES[0])
        session.put_forecast(
            keys.forecast_key(ROOM, ENTITY, ASPECT),
            issued,
            [
                (issued + datetime.timedelta(hours=h), v)
                for h, v in zip(OFFSETS_H, VALUES)
            ],
        )
        session.ready()
        while True:
            time.sleep(1)
    finally:
        session.close()


if __name__ == "__main__":
    main()
