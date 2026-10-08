# /// script
# requires-python = ">=3.11"
# dependencies = ["eclipse-zenoh~=1.10"]
# ///
"""Which series fill a house's history store, from home/history/stats.

    uv run scripts/store_profile.py                         # $HOMEOSTAT_BUS
    uv run scripts/store_profile.py --bus tcp/10.0.0.1:7447
    uv run scripts/store_profile.py --json stats.json       # a saved reply

The image ships it at /opt/homeostat/store_profile.py, where
HOMEOSTAT_BUS already points at the supervisor:

    docker exec <container> uv run /opt/homeostat/store_profile.py

The stats reply carries one entry per series. On a house with a few
hundred of them, which entity is filling the file (usually an adapter
publishing every poll rather than on change) is arithmetic nobody does
by hand: this ranks entities by rows and series by write rate.

Reads only: one get on home/history/stats, so it is safe against a live
house.
"""

import argparse
import json
import os
from collections import defaultdict


def fetch(bus: str) -> dict:
    """Return the stats reply, read over a client session."""
    import zenoh

    config = zenoh.Config()
    config.insert_json5("mode", '"client"')
    config.insert_json5("connect/endpoints", json.dumps([bus]))
    config.insert_json5("scouting/multicast/enabled", "false")
    config.insert_json5("scouting/gossip/enabled", "false")
    with zenoh.open(config) as session:
        for reply in session.get("home/history/stats", timeout=20.0):
            if reply.ok is not None:
                return json.loads(reply.ok.payload.to_bytes())
            raise SystemExit(f"stats replied with an error: {reply.err.payload.to_string()}")
    raise SystemExit("no reply from home/history/stats within 20 s")


def main() -> None:
    """Print the store's size, its entities by rows and its series by rate."""
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--bus",
        default=os.environ.get("HOMEOSTAT_BUS"),
        help="supervisor endpoint, e.g. tcp/10.0.0.1:7447 (default: $HOMEOSTAT_BUS)",
    )
    parser.add_argument("--json", type=argparse.FileType(), help="a saved stats reply")
    parser.add_argument("--top", type=int, default=15, help="rows to list (default 15)")
    args = parser.parse_args()
    if not args.bus and not args.json:
        parser.error("one of --bus or --json (or set HOMEOSTAT_BUS)")

    stats = json.load(args.json) if args.json else fetch(args.bus)
    series = stats["series"]
    total = sum(entry["rows"] for entry in series.values())
    print(f"store    {stats['file_bytes'] / 1e6:.1f} MB, layout v{stats['store_version']}")
    print(f"samples  {total:,} rows over {len(series)} series")
    print(f"events   {stats['events']['rows']:,} rows")
    if not total:
        return
    print()

    # home/history/{class}/{entity}/{aspect}, plus /{source} for a forecast.
    by_entity: dict[str, int] = defaultdict(int)
    series_of: dict[str, int] = defaultdict(int)
    for key, entry in series.items():
        space, entity = key.split("/")[2:4]
        by_entity[f"{space}/{entity}"] += entry["rows"]
        series_of[f"{space}/{entity}"] += 1

    print(f"Rows by entity (top {args.top}):")
    ranked = sorted(by_entity.items(), key=lambda kv: -kv[1])
    for name, rows in ranked[: args.top]:
        print(f"  {rows / total:5.1%}  {rows:>10,}  {name}  ({series_of[name]} series)")

    # Null for a series too short to have a rate.
    rates = sorted(
        ((entry["rows_per_day"], key) for key, entry in series.items() if entry["rows_per_day"]),
        reverse=True,
    )
    if rates:
        print(f"\nRows per day by series (top {args.top}):")
        for rate, key in rates[: args.top]:
            print(f"  {rate:12,.1f}  {key}")


if __name__ == "__main__":
    main()
