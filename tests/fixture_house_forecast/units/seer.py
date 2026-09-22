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
Repo split). This exists so the bus class, the SDK codec, the manifest
binding and the core's mirror are exercised end to end by something shaped
like the real thing — an irregular horizon whose points declare the window
they hold for, published through this unit's own `[bus.publishes]` entries
rather than around them.
"""

import datetime

from homeostat import automation

# Hourly, then three-hourly: the irregular shape real sources publish, and
# the one a regular grid could not have carried. Each point declares the
# window it holds for, as a tariff-like series does — so the last one is
# readable to its end rather than only at its start.
WINDOWS_H = [(0, 1), (1, 1), (2, 1), (3, 3), (6, 3), (9, 3)]
VALUES = [1.20, 1.45, 1.10, 0.85, 0.40, 0.95]


def main() -> None:
    ctx = automation.context()
    issued = datetime.datetime.now(datetime.timezone.utc).replace(
        minute=0, second=0, microsecond=0
    )
    # The present on the state class, the future on the forecast class:
    # one entity, one series, both reached through the declared binding
    # rather than by handing a key to the session. The forecast binding
    # names its source literally, so this call needs no slot of its own.
    ctx.publish("price_now", VALUES[0])
    ctx.publish_forecast(
        "price_forecast",
        issued,
        [
            (issued + datetime.timedelta(hours=start), v, width * 3600)
            for (start, width), v in zip(WINDOWS_H, VALUES)
        ],
    )
    ctx.ready()
    ctx.run()


if __name__ == "__main__":
    main()
