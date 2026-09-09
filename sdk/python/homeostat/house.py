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
class InputSource:
    """Where a fed device input reads from: one aspect of one entity, i.e.
    the state key home/state/{room}/{entity}/{aspect}. The room is resolved
    from the source entity's file; the entity file names only entity and
    aspect (docs/design.md, Device feeds)."""

    room: str
    entity: str
    aspect: str


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
    # [inputs]: adapter input name -> resolved source. Empty for most.
    inputs: dict[str, InputSource] = field(default_factory=dict)
    # [dashboard].pin: the entity's numeric readings are signal tiles on Now.
    pin: bool = False


@dataclass
class AdapterConfig:
    unit: str
    endpoint: str | None
    entities: list[Entity]


def load_endpoint(unit: str, root: str | Path = ".") -> str:
    """The unit's [discovery] endpoint with ${VAR} expansion. An unset
    variable is a startup error (visible via the supervisor's backoff)."""
    manifest = tomllib.loads((Path(root) / "units" / f"{unit}.toml").read_text())
    return _expand_endpoint(manifest)


def _expand_endpoint(manifest: dict) -> str:
    endpoint = os.path.expandvars(manifest["discovery"]["endpoint"])
    if "$" in endpoint:
        raise ValueError(f"unset variable in discovery endpoint: {endpoint}")
    return endpoint


def _entity_from(path: Path, data: dict, default_owner: str) -> Entity:
    # [inputs] is resolved in load_adapter, which sees every entity file.
    return Entity(
        name=path.stem,
        id=data["entity"]["id"],
        capability=data["entity"]["capability"],
        room=data["entity"]["room"],
        features=data["entity"].get("features", []),
        write_mode=data["write_policy"]["mode"],
        owner=data["write_policy"].get("owner", default_owner),
        naming=dict(data.get("naming", {})),
        pin=bool(data.get("dashboard", {}).get("pin", False)),
    )


@dataclass
class UnitInfo:
    name: str
    kind: str
    description: str = ""
    naming: dict = field(default_factory=dict)
    params: dict = field(default_factory=dict)
    publishes: dict = field(default_factory=dict)  # [bus.publishes], as declared


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
                publishes=dict(manifest.get("bus", {}).get("publishes", {})),
            )
        )
        entities_dir = manifest.get("entities", {}).get("dir")
        if entities_dir is None:
            continue
        for path in sorted((root / entities_dir).glob("*.toml")):
            entities.append(_entity_from(path, tomllib.loads(path.read_text()), unit["name"]))
    return HouseModel(zones=zones, units=units, entities=entities)


def load_adapter(unit: str, root: str | Path = ".") -> AdapterConfig:
    root = Path(root)
    manifest_path = root / "units" / f"{unit}.toml"
    manifest = tomllib.loads(manifest_path.read_text())

    # mDNS-discovery adapters (e.g. ESPHome) resolve each device's address
    # individually and declare no [discovery].endpoint; only the static
    # (single-endpoint) adapters need this populated.
    endpoint = (
        _expand_endpoint(manifest) if "endpoint" in manifest.get("discovery", {}) else None
    )

    entities = []
    entities_dir = root / manifest["entities"]["dir"]
    for path in sorted(entities_dir.glob("*.toml")):
        data = tomllib.loads(path.read_text())
        entity = _entity_from(path, data, unit)
        if data.get("inputs"):
            # Resolve each source's room from the house's entity files; the
            # plan has already validated that the entity exists.
            rooms = {e.name: e.room for e in load_house(root).entities}
            entity.inputs = {
                name: InputSource(room=rooms[src["entity"]], entity=src["entity"], aspect=src["aspect"])
                for name, src in data["inputs"].items()
            }
        entities.append(entity)
    return AdapterConfig(unit=unit, endpoint=endpoint, entities=entities)
