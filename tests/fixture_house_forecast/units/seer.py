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

# Hourly, then three-hourly: the irregular shape real sources publish, and
# the one a regular grid could not have carried. Each point declares the
# window it holds for, as a tariff-like series does — so the last one is
# readable to its end rather than only at its start.
WINDOWS_H = [(0, 1), (1, 1), (2, 1), (3, 3), (6, 3), (9, 3)]
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
                (issued + datetime.timedelta(hours=start), v, width * 3600)
                for (start, width), v in zip(WINDOWS_H, VALUES)
            ],
        )
        session.ready()
        while True:
            time.sleep(1)
    finally:
        session.close()


if __name__ == "__main__":
    main()
