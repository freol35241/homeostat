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


@dataclass
class AdapterConfig:
    unit: str
    endpoint: str
    entities: list[Entity]


def load_adapter(unit: str, root: str | Path = ".") -> AdapterConfig:
    root = Path(root)
    manifest_path = root / "units" / f"{unit}.toml"
    manifest = tomllib.loads(manifest_path.read_text())

    endpoint = os.path.expandvars(manifest["discovery"]["endpoint"])
    if "$" in endpoint:
        raise ValueError(f"unset variable in discovery endpoint: {endpoint}")

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
