# /// script
# requires-python = ">=3.11"
# dependencies = ["eclipse-zenoh~=1.9"]
# ///
"""What is actually in a house's history store, from home/history/stats.

    uv run scripts/store_profile.py --bus tcp/10.0.0.1:7447
    uv run scripts/store_profile.py --json stats.json   # a saved reply

An owner question the recorder answers but does not interpret: the stats
reply carries one entry per series, and on a house with a few hundred of
them the shape of the file -- which device is filling it, how many
retention rules it would take to control it, whether write rates separate
into "chatty" and "normal" at all -- is arithmetic nobody does by hand.
It was done by hand once (#123, where a heat pump turned out to be 89 %
of the rows) and that is the reason this exists.

Reads only: one get on home/history/stats. Nothing is written to the bus
or the store, so it is safe against a live house.

The rate it prints is computed here rather than read from the reply, so
this works against a store predating the recorder's own rows_per_day
(#137).
"""

import argparse
import datetime
import json
from collections import defaultdict

DAY = 86400.0


def rows_per_day(entry: dict) -> float | None:
    """Rows over the span they cover, or None for a series too short to
    have a rate (one row, or every row inside one instant)."""
    try:
        oldest = datetime.datetime.fromisoformat(entry["oldest"])
        newest = datetime.datetime.fromisoformat(entry["newest"])
    except (KeyError, ValueError):
        return None
    span = (newest - oldest).total_seconds()
    return entry["rows"] * DAY / span if span > 0 else None


def fetch(bus: str) -> dict:
    """The stats reply, over a client session like any other bus reader."""
    import zenoh

    config = zenoh.Config()
    config.insert_json5("mode", '"client"')
    config.insert_json5("connect/endpoints", json.dumps([bus]))
    config.insert_json5("scouting/multicast/enabled", "false")
    config.insert_json5("scouting/gossip/enabled", "false")
    with zenoh.open(config) as session:
        for reply in session.get("home/history/stats", timeout=20.0):
            result = reply.result()
            if hasattr(result, "payload"):
                return json.loads(result.payload.to_bytes())
            raise SystemExit(f"stats replied with an error: {result.payload.to_string()}")
    raise SystemExit("no reply from home/history/stats within 20 s")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--bus", help="supervisor endpoint, e.g. tcp/10.0.0.1:7447")
    parser.add_argument("--json", type=argparse.FileType(), help="a saved stats reply")
    parser.add_argument("--top", type=int, default=15, help="entities to list (default 15)")
    args = parser.parse_args()
    if not args.bus and not args.json:
        parser.error("one of --bus or --json")

    stats = json.load(args.json) if args.json else fetch(args.bus)
    series = stats["series"]
    total = sum(entry["rows"] for entry in series.values())
    if not total:
        raise SystemExit("the store has no samples")

    print(f"store    {stats['file_bytes'] / 1e6:.1f} MB, layout v{stats['store_version']}")
    print(f"samples  {total:,} rows over {len(series)} series")
    print(f"events   {stats['events']['rows']:,} rows\n")

    by_entity: dict[str, int] = defaultdict(int)
    aspects_of: dict[str, dict[str, int]] = defaultdict(dict)
    for key, entry in series.items():
        _, _, space, entity, aspect = key.split("/", 4)
        by_entity[f"{space}/{entity}"] += entry["rows"]
        aspects_of[f"{space}/{entity}"][aspect] = entry["rows"]
    ranked = sorted(by_entity.items(), key=lambda kv: -kv[1])

    print(f"Rows by entity (top {args.top}):")
    for name, rows in ranked[: args.top]:
        print(f"  {rows / total:5.1%}  {rows:>10,}  {name}  ({len(aspects_of[name])} aspects)")

    # How much of the file one rule per entity would reach: the question
    # behind "are per-series retention windows worth a config surface".
    print("\nEntity-level rules needed to cover a share of all rows:")
    covered, mark = 0, 0
    marks = [0.50, 0.80, 0.90, 0.95, 0.99]
    for n, (_, rows) in enumerate(ranked, 1):
        covered += rows
        while mark < len(marks) and covered / total >= marks[mark]:
            print(f"  {marks[mark]:>4.0%}  {n} rule{'s' if n > 1 else ''}")
            mark += 1

    # Whether the worst entity is uniform across its aspects (an entity
    # rule is enough) or concentrated (per-aspect rules would pay).
    worst, worst_rows = ranked[0]
    inside = sorted(aspects_of[worst].items(), key=lambda kv: -kv[1])
    print(f"\nInside {worst} ({worst_rows:,} rows, {len(inside)} aspects):")
    top_share = inside[0][1] / worst_rows
    print(f"  busiest  {inside[0][0]!r}: {top_share:.1%} of the entity's rows")
    print(f"  quietest {inside[-1][0]!r}: {inside[-1][1] / worst_rows:.1%}")
    spread = top_share * len(inside)
    verdict = "uniform; entity-level rules suffice" if spread < 2 else "skewed; per-aspect rules pay"
    print(f"  busiest is {spread:.2f}x the flat share -> {verdict}")

    rates = sorted((r for r in (rows_per_day(e) for e in series.values()) if r), reverse=True)
    if not rates:
        return
    print(f"\nWrite rate (rows/day) across {len(rates)} series with a measurable span:")
    for label, index in (
        ("max", 0),
        ("p90", int(len(rates) * 0.10)),
        ("p50", len(rates) // 2),
        ("p10", int(len(rates) * 0.90)),
        ("min", len(rates) - 1),
    ):
        print(f"  {label:>4}  {rates[index]:12,.1f}")

    # The widest step between neighbouring series: a rate threshold worth
    # having is one that sits in a gap, not in a continuum.
    gap, at = max(
        ((rates[i] / rates[i + 1], i) for i in range(len(rates) - 1) if rates[i + 1] > 0),
        default=(1.0, 0),
    )
    print(
        f"  widest gap: {gap:,.1f}x between #{at + 1} ({rates[at]:,.0f}/day)"
        f" and #{at + 2} ({rates[at + 1]:,.0f}/day)"
    )
    stable = "bimodal; a threshold in that gap is stable"
    print("  -> " + (stable if gap >= 5 else "a continuum; any threshold is arbitrary"))


if __name__ == "__main__":
    main()
