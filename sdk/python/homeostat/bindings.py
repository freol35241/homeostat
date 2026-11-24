"""Key resolution for an automation's bindings: pure, no bus.

A binding is a key expression from the unit's `[bus.subscribes]` or
`[bus.publishes]`. `expand` turns one into the expressions the unit
subscribes to, as `plan` printed them. `fill_slots` turns one into the
concrete key a publish or a history read addresses.
"""

from collections.abc import Iterable

from . import vocabulary
from .house import Entity

# The classes addressed per entity, as home/{class}/{room}/{entity}/{aspect}.
# These are the classes with slots to fill and the ones that expand.
# `arbiter` is left out because only adapters use it, and adapters use
# UnitSession directly instead of a Context.
ENTITY_ADDRESSED = tuple(c for c in vocabulary.ENTITY_ADDRESSED if c != "arbiter")


def _dedup(exprs: Iterable[str]) -> list[str]:
    """Return `exprs` without duplicates, in their original order.

    Two entities in one room expand a `{room}`-only expression identically,
    and a duplicate subscription delivers twice.
    """
    return list(dict.fromkeys(exprs))


def expand(
    expr: str, zones: dict[str, list[str]], entities: list[Entity]
) -> list[str]:
    """Expand an expression as the core does at plan time (src/expand.rs).

    `{room}`/`{entity}` templates are substituted per bound entity.
    Otherwise a zone in the room slot expands to one expression per member
    room. The two cannot both apply, since a template is not a zone name.
    Only the entity-addressed classes expand, because only they have a room
    slot to fill (src/keyspace.rs, ENTITY_ADDRESSED).
    """
    segments = expr.split("/")
    if len(segments) < 3 or segments[1] not in ENTITY_ADDRESSED:
        return [expr]
    if "{room}" in segments or "{entity}" in segments:

        def substitute(entity: Entity) -> str:
            filled = {"{room}": entity.room, "{entity}": entity.name}
            return "/".join(filled.get(seg, seg) for seg in segments)

        # A cmd path to an arbitrated entity belongs to the arbiter, not to
        # the entity's owner, so a templated cmd skips arbitrated entities.
        # Automation-owned entities cannot be arbitrated
        # (`virtual-entity-arbitrated`), so this currently excludes
        # nothing. It keeps this expansion in step with the core's.
        return _dedup(
            substitute(entity)
            for entity in entities
            if segments[1] != "cmd" or entity.write_mode != "arbitrated"
        )
    rooms = zones.get(segments[2])
    if rooms is None:
        return [expr]
    return ["/".join([*segments[:2], room, *segments[3:]]) for room in rooms]


def fill_slots(
    expr: str,
    unit: str,
    what: str,
    *,
    room: str | None,
    entity: str | None,
    aspect: str | None,
    source: str | None = None,
) -> str:
    """Return the concrete key `expr` addresses with its slots filled.

    Literal expression segments are defaults, and wildcard and template
    segments must be named. A hold key is addressed under `unit`'s name.
    A forecast key has one more slot than the others: its source, which
    says who publishes this forecast (docs/design.md#forecasts). Whether
    the declared expression covers the key is the caller's check.

    Parameters
    ----------
    expr : str
        The declared key expression.
    unit : str
        The unit's name, for a hold key.
    what : str
        What resolves the key, such as ``"publish 'lamp'"``, for the
        error messages.
    room : str or None
        The room slot; required where the expression's is a wildcard.
    entity : str or None
        The entity slot; required where the expression's is a wildcard.
    aspect : str or None
        The aspect slot; required where the expression's is a wildcard.
    source : str or None, optional
        The source slot of a forecast key.

    Returns
    -------
    str
        The concrete key.

    Raises
    ------
    ValueError
        If a slot the key needs is missing or not concrete, or a slot is
        given that the key does not have.
    """
    segments = expr.split("/")
    if segments[1] == "hold":
        # home/hold/{unit}/{room}/{entity}/{aspect}, under the unit's name.
        names = ("room", "entity", "aspect")
        defaults = dict(zip(names, segments[3:6], strict=False))
        parts = []
        for slot, given in (("room", room), ("entity", entity), ("aspect", aspect)):
            part = given if given is not None else defaults.get(slot)
            if part is None or "*" in part or "{" in part:
                raise ValueError(f"{what} needs a concrete {slot}")
            parts.append(part)
        key = "/".join(["home", "hold", unit, *parts])
    elif segments[1] in ENTITY_ADDRESSED:
        if segments[1] == "forecast":
            names = ("room", "entity", "aspect", "source")
            slots = {
                "room": room,
                "entity": entity,
                "aspect": aspect,
                "source": source,
            }
        else:
            if source is not None:
                raise ValueError(f"{what} takes no source slot")
            names = ("room", "entity", "aspect")
            slots = {"room": room, "entity": entity, "aspect": aspect}
        defaults: dict[str, str] = dict(zip(names, segments[2 : 2 + len(names)], strict=False))
        parts = []
        for slot, given in slots.items():
            part = given if given is not None else defaults.get(slot)
            if part is None or "*" in part or "{" in part:
                raise ValueError(f"{what} needs a concrete {slot}")
            parts.append(part)
        key = "/".join(["home", segments[1], *parts])
    else:
        if room or entity or aspect:
            raise ValueError(f"{what} takes no key slots")
        key = expr
    return key
