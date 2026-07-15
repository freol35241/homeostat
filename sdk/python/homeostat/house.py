"""House repo access for adapters.

An adapter learns its bindings from the same files the core validated: its
own manifest at units/{unit}.toml and the entity files in its entities dir.
The supervisor sets the unit's cwd to the house root, so paths are relative.

The discovery endpoint may reference environment variables (`${VAR}`) —
ports and credentials don't belong in the repo; expansion happens here, on
the adapter side, because endpoints are opaque to the core.
"""

import os
import tomllib
from dataclasses import dataclass, field
from pathlib import Path


@dataclass
class Entity:
    name: str  # file stem: the globally unique entity name
    id: str  # adapter-native address (for z2m: the topic segment)
    capability: str
    room: str
    features: list[str] = field(default_factory=list)
    write_mode: str = "shared"
    owner: str = ""
    naming: dict = field(default_factory=dict)


@dataclass
class AdapterConfig:
    unit: str
    endpoint: str
    entities: list[Entity]


def load_endpoint(unit: str, root: str | Path = ".") -> str:
    """The unit's [discovery] endpoint with ${VAR} expansion. An unset
    variable is a startup error (visible via the supervisor's backoff)."""
    manifest = tomllib.loads((Path(root) / "units" / f"{unit}.toml").read_text())
    endpoint = os.path.expandvars(manifest["discovery"]["endpoint"])
    if "$" in endpoint:
        raise ValueError(f"unset variable in discovery endpoint: {endpoint}")
    return endpoint


@dataclass
class UnitInfo:
    name: str
    kind: str
    description: str = ""
    naming: dict = field(default_factory=dict)
    params: dict = field(default_factory=dict)


@dataclass
class HouseModel:
    zones: dict[str, list[str]]  # zone name -> member rooms
    units: list[UnitInfo]
    entities: list[Entity]


def load_house(root: str | Path = ".") -> HouseModel:
    """The whole house as validated text: every unit manifest, every
    adapter's entity files, the zones. Read-only rendering data for
    consumers like the dashboard; the core remains the validator."""
    root = Path(root)

    zones: dict[str, list[str]] = {}
    zones_path = root / "zones.toml"
    if zones_path.exists():
        zones = dict(tomllib.loads(zones_path.read_text()).get("zones", {}))

    units: list[UnitInfo] = []
    entities: list[Entity] = []
    for manifest_path in sorted((root / "units").glob("*.toml")):
        manifest = tomllib.loads(manifest_path.read_text())
        unit = manifest["unit"]
        units.append(
            UnitInfo(
                name=unit["name"],
                kind=unit["kind"],
                description=unit.get("description", ""),
                naming=dict(manifest.get("naming", {})),
                params=dict(manifest.get("params", {})),
            )
        )
        entities_dir = manifest.get("entities", {}).get("dir")
        if entities_dir is None:
            continue
        for path in sorted((root / entities_dir).glob("*.toml")):
            data = tomllib.loads(path.read_text())
            entities.append(
                Entity(
                    name=path.stem,
                    id=data["entity"]["id"],
                    capability=data["entity"]["capability"],
                    room=data["entity"]["room"],
                    features=data["entity"].get("features", []),
                    write_mode=data["write_policy"]["mode"],
                    owner=data["write_policy"].get("owner", unit["name"]),
                    naming=dict(data.get("naming", {})),
                )
            )
    return HouseModel(zones=zones, units=units, entities=entities)


def load_adapter(unit: str, root: str | Path = ".") -> AdapterConfig:
    root = Path(root)
    manifest_path = root / "units" / f"{unit}.toml"
    manifest = tomllib.loads(manifest_path.read_text())

    endpoint = load_endpoint(unit, root)

    entities = []
    entities_dir = root / manifest["entities"]["dir"]
    for path in sorted(entities_dir.glob("*.toml")):
        data = tomllib.loads(path.read_text())
        entities.append(
            Entity(
                name=path.stem,
                id=data["entity"]["id"],
                capability=data["entity"]["capability"],
                room=data["entity"]["room"],
                features=data["entity"].get("features", []),
                write_mode=data["write_policy"]["mode"],
            )
        )
    return AdapterConfig(unit=unit, endpoint=endpoint, entities=entities)
