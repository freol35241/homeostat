"""House repo access for adapters.

An adapter learns its bindings from the same files the core validated: its
own manifest at units/{unit}.toml and the entity files in its entities dir.
The supervisor sets the unit's cwd to the house root, so paths are relative.

The discovery endpoint may reference environment variables (`${VAR}`) —
ports and credentials don't belong in the repo; expansion happens here, on
the adapter side, because endpoints are opaque to the core.
"""

import os
from dataclasses import dataclass, field
from pathlib import Path

import tomllib


@dataclass
class InputSource:
    """Where a fed device input reads from: one aspect of one entity.

    That is the state key home/state/{room}/{entity}/{aspect}. The room is
    resolved from the source entity's file; the entity file names only
    entity and aspect (docs/design.md#device-feeds).

    Attributes
    ----------
    room : str
        The source entity's room.
    entity : str
        The source entity's name.
    aspect : str
        The source aspect.
    """

    room: str
    entity: str
    aspect: str


@dataclass
class SourceRef:
    """One `[sources]` entry: a reading a computed value is derived from.

    It carries the caveat that belongs to THIS contributor rather than to
    the aspect (docs/design.md#sources).

    Attributes
    ----------
    entity : str
        The contributing entity's name.
    aspect : str
        The contributing aspect.
    note : str or None
        This contributor's caveat in words, if any.
    precision : float or None
        This contributor's precision, if declared.
    """

    entity: str
    aspect: str
    note: str | None = None
    precision: float | None = None


@dataclass
class Entity:
    """One entity, as its entity file declares it.

    Attributes
    ----------
    name : str
        The file stem: the globally unique entity name.
    id : str
        The adapter-native address; empty on an automation-owned entity.
    capability : str
        The entity's capability.
    room : str
        The room the entity is in.
    features : list of str
        The entity's declared features.
    write_mode : str
        The write policy's `mode`; "shared" when the file omits it.
    owner : str
        The unit that owns the entity; the declaring unit when not named.
    naming : dict
        The entity file's `[naming]` table.
    inputs : dict of str to InputSource
        `[inputs]`, resolved; filled in by `load_adapter` only.
    sources : dict of str to SourceRef
        `[sources]`: contributor name to the reading the value derives from.
    """

    name: str  # file stem: the globally unique entity name
    # Adapter-native address (for z2m: the topic segment). Empty on an
    # automation-owned entity, which has no periphery to address.
    id: str
    capability: str
    room: str
    features: list[str] = field(default_factory=list)
    write_mode: str = "shared"
    owner: str = ""
    naming: dict = field(default_factory=dict)
    # [inputs]: adapter input name -> resolved source. Empty for most.
    inputs: dict[str, InputSource] = field(default_factory=dict)
    # [sources]: contributor name -> the reading this entity's value is
    # derived from. Declared on computed entities; empty for most.
    sources: dict[str, SourceRef] = field(default_factory=dict)


@dataclass
class AdapterConfig:
    """An adapter's bindings: its unit, its discovery endpoint and its entities.

    Attributes
    ----------
    unit : str
        The adapter unit's name.
    endpoint : str or None
        The expanded `[discovery].endpoint`, or None for an adapter that
        declares none (e.g. one that discovers its devices over mDNS).
    entities : list of Entity
        The entities in the adapter's entities dir, with inputs resolved.
    """

    unit: str
    endpoint: str | None
    entities: list[Entity]


def load_endpoint(unit: str, root: str | Path = ".") -> str:
    """Return the unit's [discovery] endpoint with ${VAR} expansion.

    An unset variable is a startup error (visible via the supervisor's
    backoff).

    Parameters
    ----------
    unit : str
        The unit whose manifest to read.
    root : str or Path, optional
        The house root.

    Returns
    -------
    str
        The endpoint, variables expanded.

    Raises
    ------
    ValueError
        If the endpoint references an unset variable.
    KeyError
        If the manifest declares no `[discovery].endpoint`.
    """
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
        id=data["entity"].get("id", ""),
        capability=data["entity"]["capability"],
        room=data["entity"]["room"],
        features=data["entity"].get("features", []),
        # `mode` governs commands, so an entity whose capability takes
        # none may omit it; absent it reads as shared, as the core does.
        write_mode=data["write_policy"].get("mode", "shared"),
        owner=data["write_policy"].get("owner", default_owner),
        naming=dict(data.get("naming", {})),
        sources={
            name: SourceRef(
                entity=src["entity"],
                aspect=src["aspect"],
                note=src.get("note"),
                precision=src.get("precision"),
            )
            for name, src in data.get("sources", {}).items()
        },
    )


@dataclass
class UnitInfo:
    """One unit manifest, as declared.

    Attributes
    ----------
    name : str
        The unit's name.
    kind : str
        The unit's kind.
    description : str
        The manifest's description; empty when it has none.
    naming : dict
        The manifest's `[naming]` table.
    params : dict
        `[params]`, as declared.
    publishes : dict
        `[bus.publishes]`, as declared.
    subscribes : dict
        `[bus.subscribes]`, as declared.
    """

    name: str
    kind: str
    description: str = ""
    naming: dict = field(default_factory=dict)
    params: dict = field(default_factory=dict)
    publishes: dict = field(default_factory=dict)  # [bus.publishes], as declared
    subscribes: dict = field(default_factory=dict)  # [bus.subscribes], as declared


@dataclass
class HouseModel:
    """The whole house as validated text, as `load_house` reads it.

    Attributes
    ----------
    zones : dict of str to list of str
        Zone name to member rooms.
    units : list of UnitInfo
        Every unit manifest.
    entities : list of Entity
        Every entity of every unit that declares an entities dir.
    views : list of dict or None
        dashboard.toml's `[[view]]` list as written; None without the file.
    controls : list of dict
        dashboard.toml's `[[control]]` list as written; empty without the
        file.
    """

    zones: dict[str, list[str]]  # zone name -> member rooms
    units: list[UnitInfo]
    entities: list[Entity]
    # dashboard.toml's [[view]] list as written, None without the file.
    views: list[dict] | None = None
    # dashboard.toml's [[control]] list as written: the grain each named
    # control moves in, keyed by what it controls rather than by where it
    # is drawn. Empty without the file.
    controls: list[dict] = field(default_factory=list)


def load_house(root: str | Path = ".") -> HouseModel:
    """Return the whole house as validated text.

    Every unit manifest, every adapter's entity files, the zones, the
    dashboard's views. Read-only rendering data for consumers like the
    dashboard; the core remains the validator.

    Parameters
    ----------
    root : str or Path, optional
        The house root.

    Returns
    -------
    HouseModel
        The house as its files declare it.
    """
    root = Path(root)

    zones: dict[str, list[str]] = {}
    zones_path = root / "zones.toml"
    if zones_path.exists():
        zones = dict(tomllib.loads(zones_path.read_text()).get("zones", {}))

    views: list[dict] | None = None
    controls: list[dict] = []
    views_path = root / "dashboard.toml"
    if views_path.exists():
        dashboard = tomllib.loads(views_path.read_text())
        views = list(dashboard.get("view", []))
        controls = list(dashboard.get("control", []))

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
                subscribes=dict(manifest.get("bus", {}).get("subscribes", {})),
            )
        )
        entities_dir = manifest.get("entities", {}).get("dir")
        if entities_dir is None:
            continue
        for path in sorted((root / entities_dir).glob("*.toml")):
            entities.append(_entity_from(path, tomllib.loads(path.read_text()), unit["name"]))
    return HouseModel(
        zones=zones, units=units, entities=entities, views=views, controls=controls
    )


def load_adapter(unit: str, root: str | Path = ".") -> AdapterConfig:
    """Return an adapter's bindings, read from its manifest and entity files.

    Each entity's `[inputs]` are resolved to their source's room, read from
    the whole house's entity files.

    Parameters
    ----------
    unit : str
        The adapter unit's name.
    root : str or Path, optional
        The house root.

    Returns
    -------
    AdapterConfig
        The adapter's endpoint and entities.

    Raises
    ------
    ValueError
        If the discovery endpoint references an unset variable.
    KeyError
        If the manifest declares no `[entities].dir`.
    """
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
    # Source rooms come from the whole house's entity files, parsed once,
    # the first time an entity file wires an input.
    rooms = None
    for path in sorted(entities_dir.glob("*.toml")):
        data = tomllib.loads(path.read_text())
        entity = _entity_from(path, data, unit)
        if data.get("inputs"):
            # Resolve each source's room from the house's entity files; the
            # plan has already validated that the entity exists.
            if rooms is None:
                rooms = {e.name: e.room for e in load_house(root).entities}
            entity.inputs = {
                name: InputSource(room=rooms[src["entity"]], entity=src["entity"], aspect=src["aspect"])
                for name, src in data["inputs"].items()
            }
        entities.append(entity)
    return AdapterConfig(unit=unit, endpoint=endpoint, entities=entities)
