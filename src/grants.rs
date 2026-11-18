//! The grant table and the checks that need it. A grant is a unit's publish
//! resolved against the entities it reaches. The table is the permission
//! record that `plan` shows. It is also the dependency graph that orders an
//! apply. Device feeds (`[inputs]`) and declared sources (`[sources]`) resolve
//! here too, because they ask the same question: which unit publishes the key
//! an entity names.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::error::{Code, Diagnostics, ValidationError};
use crate::expand::{publishes, subscribes, Direction, ExpandedKey};
use crate::keyspace::{KeyExpr, Segment};
use crate::manifest::{self, FamilyHold, OnLoss, Priority, UnitKind, WriteMode};
use crate::repo::{House, LoadedEntity};

/// One resolved grant: a publish expression resolved against the concrete
/// entity set. A cmd-class row is a writer: a non-adapter's cmd publish, with
/// its capability and band. A hold-class row is a holder: a unit's hold
/// publish, with its capability and band, over the arbitrated entities it may
/// hold. A state-class row is a binding: a binding unit's state publish over
/// the entities it binds. The table is also the dependency graph (unit ->
/// entities -> owner units).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    pub unit: String,
    pub publish: String,
    /// The capability a cmd publish resolves over; none on a state row.
    #[serde(default)]
    pub capability: Option<String>,
    /// The band a cmd publish leaves at; none on a state row.
    #[serde(default)]
    pub priority: Option<Priority>,
    /// The resolved key expressions, sorted. They are part of the grant's
    /// identity, so widening `.../on` to `.../**` is a grant delta even when
    /// the same entities match.
    #[serde(default)]
    pub keys: Vec<String>,
    /// Granted entities (key match + capability match), sorted by name.
    pub entities: Vec<GrantEntity>,
    /// A hold-class row: the unit may hold these entities' aspects
    /// (docs/design.md#holds). Left out of the table when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hold: bool,
    /// A holder row's `on_loss`, when the publish declares one
    /// (docs/design.md#when-a-holder-stops).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_loss: Option<OnLoss>,
}

impl Grant {
    /// A cmd-class row: a writer. State rows record bindings only.
    pub fn is_cmd(&self) -> bool {
        self.capability.is_some() && !self.hold
    }
}

/// A granted entity with the policy facts the grant table records. Every bound
/// entity sits in its owner's state row. So an entity move, a write-mode flip,
/// a capability change or a re-binding is a grant-table delta. Any grant delta
/// escalates the plan to structural (docs/design.md#tiers), and the owner sees
/// what changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantEntity {
    pub name: String,
    pub room: String,
    #[serde(default)]
    pub capability: String,
    pub write: WriteMode,
    /// The binding unit, which is the source of the walk-order edge. An
    /// adapter, or an automation for a commandable virtual entity.
    pub owner: String,
    /// The entity's family policy (docs/design.md#family-policy), in its
    /// owner's binding row only. The arbiter reads it from there live, so
    /// an edit needs no arbiter restart. Left out when empty.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub family: BTreeMap<String, BTreeMap<String, FamilyHold>>,
}

/// An aspect of another entity that a feed or a declared source names,
/// resolved to its state key and its entity's owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contributor {
    /// The contributing entity.
    pub entity: String,
    /// The contributing aspect.
    pub aspect: String,
    /// The unit that binds the contributing entity.
    pub owner: String,
    /// The state key the aspect is read from,
    /// `home/state/{room}/{entity}/{aspect}`.
    pub key: String,
}

/// The block that names a contributor. The two raise the same errors under
/// different codes and wording.
enum Block {
    Inputs,
    Sources,
}

/// Resolves `aspect` of the entity `contributing`, which `entity` names as
/// `name` in its `[inputs]` or `[sources]`. The contributing entity must exist, and
/// an automation-owned one must publish the aspect. The key's segments come
/// back beside the contributor, for a caller that matches them against a
/// unit's subscriptions.
fn resolve_contributor<'h>(
    house: &'h House,
    expanded: &[ExpandedKey],
    entity: &LoadedEntity,
    name: &str,
    contributing: &str,
    aspect: &'h str,
    block: Block,
) -> Result<(Contributor, [&'h str; 5]), ValidationError> {
    let (unknown_entity, unpublished_aspect, entity_noun, key_noun) = match block {
        Block::Inputs => (
            Code::InputUnknownEntity,
            Code::InputUnpublishedAspect,
            "source entity",
            "a fed aspect",
        ),
        Block::Sources => (
            Code::SourceUnknownEntity,
            Code::SourceUnpublishedAspect,
            "contributing entity",
            "a declared source",
        ),
    };
    let subject = format!("{}.{name}", entity.name);
    let file = Some(entity.path.clone());
    let Some(src) = house.entities.iter().find(|e| e.name == contributing) else {
        return Err(ValidationError::new(
            unknown_entity,
            subject,
            format!("{entity_noun} \"{contributing}\" does not exist"),
            file,
        ));
    };
    let segments = ["home", "state", &src.file.entity.room, &src.name, aspect];
    let key = segments.join("/");
    let src_is_automation = house
        .unit(&src.owner)
        .is_some_and(|u| u.manifest.unit.kind == UnitKind::Automation);
    if src_is_automation && !publishes(expanded, Some(&src.owner), &segments) {
        return Err(ValidationError::new(
            unpublished_aspect,
            subject,
            format!(
                "\"{}\" does not publish {key}; {key_noun} must be in its owner's [bus.publishes]",
                src.owner
            ),
            file,
        ));
    }
    let contributor = Contributor {
        entity: src.name.clone(),
        aspect: aspect.to_string(),
        owner: src.owner.clone(),
        key,
    };
    Ok((contributor, segments))
}

/// One resolved feed: a device input wired to a source aspect
/// (docs/design.md#device-feeds). The plan renders it next to the grant table.
/// A feed is not a walk-order edge, because a control loop that reads a device
/// and feeds a term back to it is a legitimate cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Feed {
    /// The fed entity.
    pub entity: String,
    /// The adapter's name for the input.
    pub input: String,
    /// The source, resolved.
    pub source: Contributor,
}

/// Resolves every `[inputs]` block. It checks that the source exists, that the
/// target is a device, and that an automation-owned source publishes the
/// aspect.
pub fn resolve_feeds(
    house: &House,
    expanded: &[ExpandedKey],
    diags: &mut Diagnostics,
) -> Vec<Feed> {
    let mut feeds = Vec::new();
    for entity in &house.entities {
        let Some(inputs) = &entity.file.inputs else {
            continue;
        };
        let owner_is_automation = house
            .unit(&entity.owner)
            .is_some_and(|u| u.manifest.unit.kind == UnitKind::Automation);
        if owner_is_automation {
            diags.in_file(&entity.path).error(
                Code::VirtualEntityFed,
                &entity.name,
                "automation-owned entities have no device inputs to feed",
            );
            continue;
        }
        for (input, source) in inputs {
            match resolve_contributor(
                house,
                expanded,
                entity,
                input,
                &source.entity,
                &source.aspect,
                Block::Inputs,
            ) {
                Ok((contributor, _)) => feeds.push(Feed {
                    entity: entity.name.clone(),
                    input: input.clone(),
                    source: contributor,
                }),
                Err(error) => diags.push(error),
            }
        }
    }
    feeds
}

/// One resolved `[sources]` entry: a contributor to a computed value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    /// The computed entity.
    pub entity: String,
    /// The name the computed entity gives this contributor.
    pub name: String,
    /// The contributor, resolved.
    pub contributor: Contributor,
    /// This contributor's own caveat, as declared.
    pub note: Option<String>,
}

/// Checks every `[aspects]` entry against what its entity's owner publishes.
/// A misspelt aspect would otherwise sit there unused, and an interval total
/// over the real aspect would say only that it declares no interval. A unit
/// publishes only through its `[bus.publishes]`, whatever its kind, so every
/// owner is checked. An owner that publishes its entities' aspects under a
/// wildcard, as most adapters do, covers any name. A name that is not a key
/// segment is `invalid-name` already and is not checked again.
pub fn check_aspects(house: &House, expanded: &[ExpandedKey], diags: &mut Diagnostics) {
    for entity in &house.entities {
        for aspect in entity.file.aspects.iter().flat_map(|a| a.keys()) {
            let segments = [
                "home",
                "state",
                &entity.file.entity.room,
                &entity.name,
                aspect,
            ];
            let key = segments.join("/");
            if !crate::validate::valid_segment(aspect) {
                continue;
            }
            if !publishes(expanded, Some(&entity.owner), &segments) {
                diags.in_file(&entity.path).error(
                    Code::AspectUnpublished,
                    format!("{}.{aspect}", entity.name),
                    format!(
                        "\"{}\" does not publish {key}; [aspects] names an aspect its owner publishes",
                        entity.owner
                    ),
                );
            }
        }
    }
}

/// Resolves every `[sources]` block. It makes the same two checks as
/// `[inputs]`, for the same reason: the contributor exists, and an
/// automation-owned contributor publishes the aspect. It also warns when the
/// unit does not subscribe a source it claims to derive from. Such a unit is
/// either mis-declared or has dead code, and the overlay would draw a line
/// that plays no part in the value.
pub fn resolve_sources(
    house: &House,
    expanded: &[ExpandedKey],
    diags: &mut Diagnostics,
) -> Vec<Source> {
    let mut sources = Vec::new();
    for entity in &house.entities {
        let Some(declared) = &entity.file.sources else {
            continue;
        };
        for (name, source) in declared {
            let (contributor, segments) = match resolve_contributor(
                house,
                expanded,
                entity,
                name,
                &source.entity,
                &source.aspect,
                Block::Sources,
            ) {
                Ok(resolved) => resolved,
                Err(error) => {
                    diags.push(error);
                    continue;
                }
            };
            if !subscribes(expanded, Some(&entity.owner), &segments) {
                diags.in_file(&entity.path).warn(
                    format!("{}.{name}", entity.name),
                    format!(
                        "\"{}\" declares {} as a source but does not subscribe it",
                        entity.owner, contributor.key
                    ),
                );
            }
            sources.push(Source {
                entity: entity.name.clone(),
                name: name.clone(),
                contributor,
                note: source.note.clone(),
            });
        }
    }
    sources
}

/// One slot of a forecast publish (aspect or source): a literal, or any value
/// when the expression wildcards it.
#[derive(PartialEq)]
enum Slot {
    Exact(String),
    Any,
}

impl Slot {
    /// Whether two publishes can land on the same key at this slot.
    fn compatible(&self, other: &Slot) -> bool {
        match (self, other) {
            (Slot::Exact(a), Slot::Exact(b)) => a == b,
            _ => true,
        }
    }
}

/// Resolves the grant table and enforces write policy.
pub fn resolve(house: &House, expanded: &[ExpandedKey], diags: &mut Diagnostics) -> Vec<Grant> {
    let grants = build_grants(house, expanded, diags);
    check_exclusive_writers(house, &grants, diags);
    check_arbitrated_coverage(house, expanded, diags);
    check_commanded_virtuals(house, expanded, &grants, diags);
    check_grant_cycle(&grants, diags);
    check_entity_keys_bound(house, expanded, diags);
    check_forecast_conflicts(expanded, diags);
    check_reserved_classes(house, expanded, diags);
    grants
}

/// One grant per state and forecast publish (the entities its unit binds)
/// and per non-adapter cmd publish (the entities its capability and keys
/// reach), sorted by unit and binding.
fn build_grants(house: &House, expanded: &[ExpandedKey], diags: &mut Diagnostics) -> Vec<Grant> {
    let mut grants = Vec::new();

    for key in expanded {
        if key.direction != Direction::Publishes {
            continue;
        }
        // The class comes from the source, so a templated publish that
        // expanded to nothing still gets the capability checks and the
        // "matches no entities" warning.
        let class = key.class;
        let unit = house
            .unit(&key.unit)
            .expect("expanded key from loaded unit");
        let spec = &unit.manifest.bus.as_ref().expect("unit has bus").publishes[&key.entry];
        let subject = format!("{}.{}", key.unit, key.entry);
        let mut diags = diags.in_file(&unit.path);
        let mut keys: Vec<String> = key.exprs.iter().map(ToString::to_string).collect();
        keys.sort();
        if spec.on_loss.is_some() && class != "hold" {
            diags.error(
                Code::OnLossNotHold,
                subject.clone(),
                format!(
                    "publish \"{}\" declares on_loss, which only a hold publish takes",
                    key.source
                ),
            );
        }

        // A binding unit's state or forecast publish records what it binds, so
        // a change to any bound entity is a grant delta. Forecast counts for
        // the same reason as state: it is the same series extended forward,
        // onto the same entity. Leaving it out would break apply. A unit
        // publishing only a forecast onto an entity it owns would produce no
        // row, so a change to that entity would not read as a grant delta and
        // apply would not see it.
        if class == "state" || class == "forecast" {
            let mut bound: Vec<GrantEntity> = house
                .entities
                .iter()
                .filter(|e| e.owner == key.unit)
                .filter(|e| {
                    let prefix = ["home", class, e.file.entity.room.as_str(), e.name.as_str()];
                    key.reaches(Direction::Publishes, &prefix)
                })
                .map(|e| GrantEntity {
                    name: e.name.clone(),
                    room: e.file.entity.room.clone(),
                    capability: e.file.entity.capability.clone(),
                    write: e.file.write_policy.mode(),
                    owner: e.owner.clone(),
                    family: e.file.write_policy.family.clone(),
                })
                .collect();
            bound.sort_by(|a, b| a.name.cmp(&b.name));
            grants.push(Grant {
                unit: key.unit.clone(),
                publish: key.entry.clone(),
                capability: None,
                priority: None,
                keys,
                entities: bound,
                hold: false,
                on_loss: None,
            });
            continue;
        }
        let hold = class == "hold";
        // A service's hold publish is its own output, as the arbiter's
        // family holds are, and grants nothing.
        if hold && key.kind == UnitKind::Service {
            continue;
        }
        if !hold && (class != "cmd" || key.kind == UnitKind::Adapter) {
            continue;
        }

        let Some(capability) = spec.capability.clone() else {
            diags.error(
                Code::PublishMissingCapability,
                subject,
                format!(
                    "{class} publish \"{}\" must declare a capability",
                    key.source
                ),
            );
            continue;
        };
        if manifest::capability(&capability).is_none() {
            diags.error(
                Code::UnknownCapability,
                subject,
                format!("unknown capability \"{capability}\""),
            );
            continue;
        }
        let Some(priority) = spec.priority else {
            diags.error(
                Code::PublishMissingPriority,
                subject,
                format!("{class} publish \"{}\" must declare a priority", key.source),
            );
            continue;
        };
        if hold && !matches!(priority, Priority::Automation | Priority::Agent) {
            diags.error(
                Code::HoldBand,
                subject,
                format!("hold publish \"{}\" declares priority \"{priority}\"; a unit holds at automation or agent", key.source),
            );
            continue;
        }

        let mut granted: Vec<GrantEntity> = house
            .entities
            .iter()
            .filter(|e| e.file.entity.capability == capability)
            .filter(|e| {
                let (room, name) = (e.file.entity.room.as_str(), e.name.as_str());
                if hold {
                    // home/hold/{unit}/{room}/{entity}/{aspect}
                    key.reaches(
                        Direction::Publishes,
                        &["home", "hold", key.unit.as_str(), room, name],
                    )
                } else {
                    key.reaches(Direction::Publishes, &["home", "cmd", room, name])
                }
            })
            .map(|e| GrantEntity {
                name: e.name.clone(),
                room: e.file.entity.room.clone(),
                capability: capability.clone(),
                write: e.file.write_policy.mode(),
                owner: e.owner.clone(),
                family: BTreeMap::new(),
            })
            .collect();
        granted.sort_by(|a, b| a.name.cmp(&b.name));
        granted.dedup();

        if granted.is_empty() {
            diags.warn(&subject, "publish matches no entities");
        }
        if hold {
            for entity in granted.iter().filter(|e| e.write != WriteMode::Arbitrated) {
                diags.error(
                    Code::HoldNotArbitrated,
                    subject.clone(),
                    format!(
                        "hold publish \"{}\" reaches \"{}\", which is {}, not arbitrated",
                        key.source, entity.name, entity.write
                    ),
                );
            }
        }
        // The manual band belongs to the family's surfaces (dashboard, voice)
        // and is exempt from exclusive-write checks. Nothing forbids an
        // automation from claiming it, but the plan warns about it.
        if !hold && priority == Priority::Manual && unit.manifest.unit.kind != UnitKind::Service {
            diags.warn(
                &subject,
                format!(
                    "publish declares priority \"manual\" on {} \"{}\"; the manual band is the family's and is exempt from exclusive-write checks",
                    unit.manifest.unit.kind, key.unit
                ),
            );
        }
        grants.push(Grant {
            unit: key.unit.clone(),
            publish: key.entry.clone(),
            capability: Some(capability),
            priority: Some(priority),
            keys,
            entities: granted,
            hold,
            on_loss: spec.on_loss,
        });
    }

    grants.sort_by(|a, b| (&a.unit, &a.publish).cmp(&(&b.unit, &b.publish)));
    warn_same_rank_holders(&grants, diags);
    grants
}

/// Per held entity and band: each holder, its entity's capability, and the
/// aspect each of its keys reaches (None for a wildcard).
type HolderRows<'a> = Vec<(&'a str, &'a str, Vec<Option<String>>)>;

/// Two units holding one aspect at one band are told apart by the
/// aspect's rest value (docs/design.md#which-hold-wins). Without one, the
/// newest hold wins, so the outcome depends only on timing, and plan says
/// so.
fn warn_same_rank_holders(grants: &[Grant], diags: &mut Diagnostics) {
    // The aspect each of a row's keys holds on an entity: a literal, or
    // None where a wildcard covers any aspect.
    let aspects = |grant: &Grant, entity: &GrantEntity| -> Vec<Option<String>> {
        let prefix = [
            "home",
            "hold",
            grant.unit.as_str(),
            entity.room.as_str(),
            entity.name.as_str(),
        ];
        grant
            .keys
            .iter()
            .filter_map(|raw| KeyExpr::parse(raw).ok())
            .filter(|expr| expr.matches_prefix(&prefix))
            .map(|expr| match expr.0.as_slice() {
                [_, _, _, _, _, Segment::Literal(aspect)] => Some(aspect.clone()),
                _ => None,
            })
            .collect()
    };
    let mut holders: BTreeMap<(&str, String), HolderRows> = BTreeMap::new();
    for grant in grants.iter().filter(|g| g.hold) {
        let Some(priority) = grant.priority else {
            continue;
        };
        for entity in &grant.entities {
            holders
                .entry((entity.name.as_str(), priority.to_string()))
                .or_default()
                .push((
                    grant.unit.as_str(),
                    entity.capability.as_str(),
                    aspects(grant, entity),
                ));
        }
    }
    for ((entity, priority), rows) in holders {
        let rest = rows
            .first()
            .and_then(|(_, capability, _)| manifest::capability(capability))
            .and_then(|c| c.rest)
            .map(|(aspect, _)| aspect);
        // Two keys share an aspect when both name it, or one is a wildcard.
        // Sharing only the rest aspect is decided by the rest value.
        let clash = |a: &Option<String>, b: &Option<String>| match (a, b) {
            (Some(x), Some(y)) => x == y && Some(x.as_str()) != rest,
            (Some(x), None) | (None, Some(x)) => Some(x.as_str()) != rest,
            (None, None) => true,
        };
        let mut units: BTreeSet<&str> = BTreeSet::new();
        for (i, (unit_a, _, keys_a)) in rows.iter().enumerate() {
            for (unit_b, _, keys_b) in &rows[i + 1..] {
                if unit_a != unit_b && keys_a.iter().any(|a| keys_b.iter().any(|b| clash(a, b))) {
                    units.insert(unit_a);
                    units.insert(unit_b);
                }
            }
        }
        if units.len() > 1 {
            let names: Vec<&str> = units.into_iter().collect();
            diags.warn(
                entity,
                format!(
                    "{} hold it at {priority} on an aspect without a rest value: the newest hold wins",
                    names.join(" and ")
                ),
            );
        }
    }
}

/// Write-policy enforcement: two writers on an exclusive entity is an error.
/// Exclusivity constrains only the bands below manual. Manual-band units
/// (dashboard, voice) sit above it and do not count. A writer is a unit rather
/// than a binding, because authority is per process.
fn check_exclusive_writers(house: &House, grants: &[Grant], diags: &mut Diagnostics) {
    let mut writers: BTreeMap<&str, BTreeMap<&str, Vec<String>>> = BTreeMap::new();
    for grant in grants {
        if matches!(grant.priority, None | Some(Priority::Manual)) {
            continue;
        }
        for entity in &grant.entities {
            writers
                .entry(entity.name.as_str())
                .or_default()
                .entry(grant.unit.as_str())
                .or_default()
                .push(format!("{}.{}", grant.unit, grant.publish));
        }
    }
    for entity in &house.entities {
        if entity.file.write_policy.mode() != WriteMode::Exclusive {
            continue;
        }
        if let Some(writers) = writers.get(entity.name.as_str()) {
            if writers.len() > 1 {
                let bindings: Vec<&str> = writers.values().flatten().map(String::as_str).collect();
                diags.in_file(&entity.path).error(
                    Code::ExclusiveWriteConflict,
                    &entity.name,
                    format!(
                        "exclusive entity has {} writers: {}",
                        writers.len(),
                        bindings.join(", ")
                    ),
                );
            }
        }
    }
}

/// Arbitration coverage. An arbitrated entity that no arbiter-class publish
/// reaches would never receive a write token, because the arbiter service has
/// no path to it. This mirrors `check_exclusive_writers`, but works over
/// `expanded` directly, since arbiter-class publishes do not form cmd-class
/// grants.
fn check_arbitrated_coverage(house: &House, expanded: &[ExpandedKey], diags: &mut Diagnostics) {
    for entity in &house.entities {
        if entity.file.write_policy.mode() != WriteMode::Arbitrated {
            continue;
        }
        let prefix = [
            "home",
            "arbiter",
            entity.file.entity.room.as_str(),
            entity.name.as_str(),
        ];
        if !publishes(expanded, None, &prefix) {
            diags.in_file(&entity.path).error(
                Code::ArbitratedUncovered,
                &entity.name,
                format!(
                    "arbitrated entity \"{}\" has no arbiter-class publish covering it",
                    entity.name
                ),
            );
        }
    }
}

/// A commandable virtual entity is a latch
/// (docs/design.md#commandable-virtual-entities). Its owning automation
/// subscribes to the entity's cmd keys and sets its own state. A cmd-class
/// grant onto an automation-owned entity whose owner does not subscribe would
/// send commands that nobody receives, so it is refused.
fn check_commanded_virtuals(
    house: &House,
    expanded: &[ExpandedKey],
    grants: &[Grant],
    diags: &mut Diagnostics,
) {
    for grant in grants.iter().filter(|g| g.is_cmd()) {
        for granted in &grant.entities {
            let name = &granted.name;
            let Some(entity) = house.entities.iter().find(|e| &e.name == name) else {
                continue;
            };
            let automation_owned = house
                .unit(&entity.owner)
                .is_some_and(|u| u.manifest.unit.kind == UnitKind::Automation);
            if !automation_owned {
                continue;
            }
            let room = entity.file.entity.room.as_str();
            let prefix = ["home", "cmd", room, name.as_str()];
            if !subscribes(expanded, Some(&entity.owner), &prefix) {
                diags.in_file(&entity.path).error(
                    Code::VirtualEntityCommanded,
                    name,
                    format!(
                        "\"{name}\" is bound by automation \"{}\", which subscribes to no home/cmd/{room}/{name} keys (cmd publish {}.{})",
                        entity.owner, grant.unit, grant.publish
                    ),
                );
            }
        }
    }
}

/// Edges (owner, dependent) from a grant table: the granted entities' owner
/// units must be up before the granting unit. The owner is stored in the grant
/// itself, so edges hold even for entities the repo no longer declares. Owners
/// are adapters, or automations for commandable virtual entities
/// (docs/design.md#commandable-virtual-entities).
pub fn grant_edges(grants: &[Grant]) -> Vec<(String, String)> {
    let mut edges = Vec::new();
    for grant in grants {
        for entity in &grant.entities {
            if entity.owner != grant.unit {
                edges.push((entity.owner.clone(), grant.unit.clone()));
            }
        }
    }
    edges
}

/// The nodes of `remaining` that no other node still in `remaining` has an
/// edge to: the next layer of a topological sort. An empty result while
/// `remaining` is not empty means every node left is on, or behind, a cycle.
pub fn free_nodes(remaining: &BTreeSet<String>, edges: &[(String, String)]) -> Vec<String> {
    remaining
        .iter()
        .filter(|unit| {
            !edges
                .iter()
                .any(|(a, d)| d == *unit && a != *unit && remaining.contains(a))
        })
        .cloned()
        .collect()
}

/// Grant edges run from owner to granting unit. With automations as owners a
/// cycle is possible: A commands an entity B binds while B commands one A
/// binds. The apply walk needs an order, so the house is refused at plan time
/// instead of starting units in an arbitrary order.
fn check_grant_cycle(grants: &[Grant], diags: &mut Diagnostics) {
    let edges = grant_edges(grants);
    let mut remaining: BTreeSet<String> = edges
        .iter()
        .flat_map(|(a, d)| [a.clone(), d.clone()])
        .collect();
    loop {
        let free = free_nodes(&remaining, &edges);
        if free.is_empty() {
            break;
        }
        for unit in free {
            remaining.remove(&unit);
        }
    }
    if !remaining.is_empty() {
        let members: Vec<String> = remaining.into_iter().collect();
        diags.error(
            Code::GrantCycle,
            members.join(", "),
            "each of these units commands an entity another of them binds, so no apply order exists",
        );
    }
}

/// An entity-addressed key must belong to a bound entity. A templated publish
/// always does. A concrete one must name a bound entity's room and name
/// literally. Without this check a virtual sensor could publish under any
/// free-form key. `forecast` follows the same rule as `state`, because a
/// forecast extends the same series forward and so uses the same entity's key
/// space.
fn check_entity_keys_bound(house: &House, expanded: &[ExpandedKey], diags: &mut Diagnostics) {
    for key in expanded {
        if key.direction != Direction::Publishes || key.templated {
            continue;
        }
        for expr in &key.exprs {
            let Some(class @ ("state" | "forecast")) = expr.class() else {
                continue;
            };
            let forecast = class == "forecast";
            let code = if forecast {
                Code::ForecastPublishUnbound
            } else {
                Code::StatePublishUnbound
            };
            let unit = house
                .unit(&key.unit)
                .expect("expanded key from loaded unit");
            let subject = format!("{}.{}", key.unit, key.entry);
            let (room, entity) = (expr.0.get(2), expr.0.get(3));
            let (Some(Segment::Literal(room)), Some(Segment::Literal(entity))) = (room, entity)
            else {
                let message = format!(
                    "{class} publish \"{}\" needs literal room and entity segments (or {{room}}/{{entity}} templates)",
                    key.source
                );
                diags.in_file(&unit.path).error(code, subject, message);
                continue;
            };
            // State needs the binding unit, because each entity has one master
            // and the registry records it. A forecast does not. It is a
            // source's claim about a series' future values, and the best
            // source is often not the binder. A weather service forecasts an
            // outdoor sensor it does not own. A controller forecasts the
            // trajectory of a device it commands, and the grant graph means it
            // cannot also bind that device. The entity must still exist, so
            // that the recorder and the dashboard show the value.
            let declared = house
                .entities
                .iter()
                .find(|e| &e.name == entity && &e.file.entity.room == room);
            let bound = match declared {
                None => false,
                Some(e) => forecast || e.owner == key.unit,
            };
            if !bound {
                let message = if forecast {
                    format!(
                        "forecast key \"home/forecast/{room}/{entity}/…\" names no entity in the house"
                    )
                } else {
                    format!(
                        "state key \"home/state/{room}/{entity}/…\" is not under an entity bound by \"{}\"",
                        key.unit
                    )
                };
                diags.in_file(&unit.path).error(code, subject, message);
            }
        }
    }
}

/// Each forecast series has one publisher, where "series" includes the source.
/// Several providers may forecast one aspect, which is what the source segment
/// is for. Two units must not write the same source's key: the mirror keeps
/// only the last document per key, so the second would overwrite the first.
///
/// A publish may wildcard its aspect or source slot, so entries are compared
/// pairwise instead of grouped by key. Two publishes collide when, at every
/// slot, they agree or one of them accepts anything.
fn check_forecast_conflicts(expanded: &[ExpandedKey], diags: &mut Diagnostics) {
    let slot = |expr: &KeyExpr, i: usize| -> Slot {
        // `**` anywhere from the aspect slot on stands for every slot
        // after it, so a key that has one accepts anything here.
        if expr.0.iter().skip(4).any(|s| matches!(s, Segment::AnyRec)) {
            return Slot::Any;
        }
        match expr.0.get(i) {
            Some(Segment::Literal(v)) => Slot::Exact(v.clone()),
            _ => Slot::Any,
        }
    };
    let mut forecast_publishes: Vec<(&str, String, String, Slot, Slot)> = Vec::new();
    for key in expanded
        .iter()
        .filter(|k| k.direction == Direction::Publishes)
    {
        for expr in &key.exprs {
            if expr.class() != Some("forecast") {
                continue;
            }
            let (Some(Segment::Literal(room)), Some(Segment::Literal(entity))) =
                (expr.0.get(2), expr.0.get(3))
            else {
                continue;
            };
            forecast_publishes.push((
                key.unit.as_str(),
                room.clone(),
                entity.clone(),
                slot(expr, 4),
                slot(expr, 5),
            ));
        }
    }
    let mut reported: BTreeSet<(&str, &str)> = BTreeSet::new();
    for (i, a) in forecast_publishes.iter().enumerate() {
        for b in forecast_publishes.iter().skip(i + 1) {
            if a.0 == b.0 || a.1 != b.1 || a.2 != b.2 {
                continue;
            }
            if !a.3.compatible(&b.3) || !a.4.compatible(&b.4) {
                continue;
            }
            let pair = if a.0 < b.0 { (a.0, b.0) } else { (b.0, a.0) };
            if !reported.insert(pair) {
                continue;
            }
            diags.error(
                Code::ForecastPublishConflict,
                format!("home/forecast/{}/{}", a.1, a.2),
                format!(
                    "{} and {} publish forecasts that land on the same key; \
                     give each source its own segment, or one overwrites the other",
                    pair.0, pair.1
                ),
            );
        }
    }
}

/// Reserved classes (docs/design.md#reserved-classes). `config` and `meta`
/// belong to the core. `health`, `discovery`, `hold` and `arbitration` are
/// per unit, under
/// the publishing unit's own name. `arbiter`, `clock` and `history` are each
/// one service's output. The SDK only checks that a published key is within a
/// declared expression. Without this check an automation could declare
/// `home/arbiter/**` and forge post-arbitration commands, or another unit's
/// discovery record or holds, with an empty grant table.
fn check_reserved_classes(house: &House, expanded: &[ExpandedKey], diags: &mut Diagnostics) {
    let mut singleton_publishers: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for key in expanded
        .iter()
        .filter(|k| k.direction == Direction::Publishes)
    {
        if matches!(key.class, "arbiter" | "clock" | "history") {
            singleton_publishers
                .entry(key.class)
                .or_default()
                .insert(key.unit.as_str());
        }
    }
    for key in expanded
        .iter()
        .filter(|k| k.direction == Direction::Publishes)
    {
        let class = key.class;
        let next = key.source.split('/').nth(2);
        let unit = house
            .unit(&key.unit)
            .expect("expanded key from loaded unit");
        let message = match class {
            "config" | "meta" => Some(format!(
                "\"{}\" publishes under home/{class}/, which only the core writes",
                key.source
            )),
            "health" | "discovery" | "hold" | "arbitration" if next != Some(key.unit.as_str()) => {
                Some(format!(
                    "\"{}\" must sit under this unit's own name: home/{class}/{}/...",
                    key.source, key.unit
                ))
            }
            "arbiter" | "clock" | "history" => {
                let publishers = &singleton_publishers[class];
                if unit.manifest.unit.kind != UnitKind::Service {
                    Some(format!(
                        "\"{}\": only a service may publish under home/{class}/",
                        key.source
                    ))
                } else if publishers.len() > 1 {
                    let others: Vec<&str> = publishers
                        .iter()
                        .copied()
                        .filter(|u| *u != key.unit)
                        .collect();
                    Some(format!(
                        "\"{}\": home/{class}/ is also published by {}; exactly one service owns it",
                        key.source,
                        others.join(", ")
                    ))
                } else {
                    None
                }
            }
            _ => None,
        };
        if let Some(message) = message {
            diags.in_file(&unit.path).error(
                Code::ReservedClassPublish,
                format!("{}.{}", key.unit, key.entry),
                message,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Warning;
    use crate::expand::expand;

    /// Runs one stage into a fresh sink and returns its output beside the
    /// warnings and errors it reported.
    fn collect<T>(
        stage: impl FnOnce(&mut Diagnostics) -> T,
    ) -> (T, Vec<Warning>, Vec<ValidationError>) {
        let mut diags = Diagnostics::default();
        let out = stage(&mut diags);
        (out, diags.warnings, diags.errors)
    }

    const ZIGBEE: &str = r#"
        schema = 1

        [unit]
        name = "zigbee"
        kind = "adapter"

        [runtime]
        command = "true"
        restart = "always"

        [entities]
        dir = "entities/zigbee/"

        [bus.subscribes]
        commands = "home/cmd/{room}/{entity}/**"
        arbiter_commands = "home/arbiter/{room}/{entity}/**"
    "#;

    const NIGHT_MODE: &str = r#"
        schema = 1

        [unit]
        name = "night_mode"
        kind = "automation"

        [runtime]
        command = "true"
        restart = "always"

        [bus.publishes.lock]
        key = "home/cmd/hallway/lock/lock"
        capability = "lock"
        priority = "automation"
    "#;

    const ARBITER: &str = r#"
        schema = 1

        [unit]
        name = "arbiter"
        kind = "service"

        [runtime]
        command = "true"
        restart = "always"

        # A service (not an adapter) cannot use {room}/{entity} templates, so
        # it covers arbitrated entities with a plain wildcard instead.
        [bus.publishes.forwarded]
        key = "home/arbiter/**"
    "#;

    const LOCK_ENTITIES: &[(&str, &str, &str)] = &[
        (
            "zigbee",
            "lamp",
            r#"
                schema = 1

                [entity]
                id = "lamp"
                capability = "light"
                room = "kitchen"

                [write_policy]
                mode = "shared"
                owner = "zigbee"
            "#,
        ),
        (
            "zigbee",
            "lock",
            r#"
                schema = 1

                [entity]
                id = "lock"
                capability = "lock"
                room = "hallway"

                [write_policy]
                mode = "arbitrated"
                owner = "zigbee"
            "#,
        ),
    ];

    /// Builds a house with an arbitrated lock and a shared lamp, both bound
    /// to adapter "zigbee", plus an automation wishing to command the lock.
    /// `arbiter` lets each test decide whether an arbiter-class publish
    /// covers the lock.
    fn house_with_lock(arbiter: bool) -> House {
        let mut units = vec![("zigbee", ZIGBEE), ("night_mode", NIGHT_MODE)];
        if arbiter {
            units.push(("arbiter", ARBITER));
        }
        House::from_toml(&units, LOCK_ENTITIES)
    }

    #[test]
    fn arbitrated_entity_covered_by_arbiter_publish_has_no_errors() {
        let house = house_with_lock(true);

        let (expanded, _warnings, expand_errors) = collect(|d| expand(&house, d));
        assert!(expand_errors.is_empty(), "{expand_errors:?}");

        // Correct expansion split: the adapter's cmd template excludes the
        // arbitrated lock, its arbiter template includes only the lock.
        let cmd = expanded.iter().find(|k| k.entry == "commands").unwrap();
        assert_eq!(
            cmd.exprs,
            vec![KeyExpr::parse("home/cmd/kitchen/lamp/**").unwrap()]
        );
        let arbiter_cmd = expanded
            .iter()
            .find(|k| k.entry == "arbiter_commands")
            .unwrap();
        assert_eq!(
            arbiter_cmd.exprs,
            vec![KeyExpr::parse("home/arbiter/hallway/lock/**").unwrap()]
        );

        let (grants, warnings, errors) = collect(|d| resolve(&house, &expanded, d));
        assert!(errors.is_empty(), "{errors:?}");
        assert!(warnings.is_empty(), "{warnings:?}");
        let night_mode = grants.iter().find(|g| g.unit == "night_mode").unwrap();
        assert_eq!(
            night_mode.entities,
            vec![GrantEntity {
                name: "lock".to_string(),
                room: "hallway".to_string(),
                capability: "lock".to_string(),
                write: WriteMode::Arbitrated,
                owner: "zigbee".to_string(),
                family: BTreeMap::new(),
            }]
        );
    }

    /// A room move or a write-mode flip changes the resolved grant table, so
    /// it diffs as a grant delta. This is what makes entity moves and
    /// write-policy changes structural (docs/design.md#tiers).
    #[test]
    fn entity_move_and_policy_flip_change_the_grant_table() {
        let baseline = house_with_lock(true);
        let (expanded, _, _) = collect(|d| expand(&baseline, d));
        let (grants, _, _) = collect(|d| resolve(&baseline, &expanded, d));

        let mut moved = house_with_lock(true);
        moved.entities[1].file.entity.room = "porch".to_string();
        // The publish key still names the old room, so re-expansion changes
        // the granted set. The tables differ either way.
        let (expanded, _, _) = collect(|d| expand(&moved, d));
        let (moved_grants, _, _) = collect(|d| resolve(&moved, &expanded, d));
        assert_ne!(
            grants, moved_grants,
            "a room move must change the grant table"
        );

        let mut flipped = house_with_lock(true);
        flipped.entities[0].file.write_policy.mode = Some(WriteMode::Exclusive);
        // The lamp is granted to nobody; flip the lock instead, which
        // night_mode writes.
        flipped.entities[1].file.write_policy.mode = Some(WriteMode::Shared);
        let (expanded, _, _) = collect(|d| expand(&flipped, d));
        let (flipped_grants, _, _) = collect(|d| resolve(&flipped, &expanded, d));
        assert_ne!(
            grants, flipped_grants,
            "a write-mode flip must change the grant table"
        );
    }

    /// A latch: automation "modes" binds a switch, subscribes to its cmd
    /// keys and publishes its state; automation "buttons" commands it.
    /// `listens` decides whether modes declares the cmd subscription.
    fn house_with_latch(listens: bool) -> House {
        let mut modes = String::from(
            r#"
            schema = 1

            [unit]
            name = "modes"
            kind = "automation"

            [runtime]
            command = "true"
            restart = "always"

            [entities]
            dir = "entities/modes/"

            [bus.publishes.state]
            key = "home/state/{room}/{entity}/**"

            [bus.subscribes]
        "#,
        );
        if listens {
            modes.push_str(r#"commands = "home/cmd/{room}/{entity}/**""#);
        }
        let buttons = r#"
            schema = 1

            [unit]
            name = "buttons"
            kind = "automation"

            [runtime]
            command = "true"
            restart = "always"

            [bus.publishes.mode]
            key = "home/cmd/global/house_mode/on"
            capability = "switch"
            priority = "automation"
        "#;
        let house_mode = r#"
            schema = 1

            [entity]
            capability = "switch"
            room = "global"

            [write_policy]
            mode = "shared"
            owner = "modes"
        "#;
        House::from_toml(
            &[("modes", &modes), ("buttons", buttons)],
            &[("modes", "house_mode", house_mode)],
        )
    }

    #[test]
    fn a_commanded_virtual_entity_whose_owner_listens_is_a_latch() {
        let house = house_with_latch(true);
        let (expanded, _warnings, expand_errors) = collect(|d| expand(&house, d));
        assert!(expand_errors.is_empty(), "{expand_errors:?}");

        let (grants, warnings, errors) = collect(|d| resolve(&house, &expanded, d));
        assert!(errors.is_empty(), "{errors:?}");
        assert!(warnings.is_empty(), "{warnings:?}");
        let buttons = grants.iter().find(|g| g.unit == "buttons").unwrap();
        // The owner is the automation, so the walk-order edge starts there.
        assert_eq!(
            buttons.entities,
            vec![GrantEntity {
                name: "house_mode".to_string(),
                room: "global".to_string(),
                capability: "switch".to_string(),
                write: WriteMode::Shared,
                owner: "modes".to_string(),
                family: BTreeMap::new(),
            }]
        );
    }

    #[test]
    fn a_commanded_virtual_entity_nobody_listens_for_is_a_plan_error() {
        let house = house_with_latch(false);
        let (expanded, _warnings, expand_errors) = collect(|d| expand(&house, d));
        assert!(expand_errors.is_empty(), "{expand_errors:?}");

        let (_grants, _warnings, errors) = collect(|d| resolve(&house, &expanded, d));
        let codes: Vec<Code> = errors.iter().map(|e| e.code).collect();
        assert_eq!(codes, vec![Code::VirtualEntityCommanded], "{errors:?}");
    }

    /// The key expression is part of a grant's identity. Widening `/lock` to
    /// `/**` matches the same entity but grants a different authority, so it
    /// must diff as a grant delta and show in the plan.
    #[test]
    fn widening_a_key_expression_changes_the_grant_table() {
        let house = house_with_lock(false);
        let (expanded, _, _) = collect(|d| expand(&house, d));
        let (grants, _, _) = collect(|d| resolve(&house, &expanded, d));
        let night_mode = grants.iter().find(|g| g.unit == "night_mode").unwrap();
        assert_eq!(
            night_mode.keys,
            vec!["home/cmd/hallway/lock/lock".to_string()]
        );

        let mut widened = house_with_lock(false);
        let bus = night_mode_bus(&mut widened);
        bus.publishes.get_mut("lock").unwrap().key = "home/cmd/hallway/lock/**".to_string();
        let (expanded, _, _) = collect(|d| expand(&widened, d));
        let (widened_grants, _, _) = collect(|d| resolve(&widened, &expanded, d));
        let widened_night_mode = widened_grants
            .iter()
            .find(|g| g.unit == "night_mode")
            .unwrap();
        assert_eq!(
            widened_night_mode.entities, night_mode.entities,
            "same entity either way"
        );
        assert_ne!(
            grants, widened_grants,
            "a wider key expression must change the grant table"
        );
    }

    /// A forecast publish binds its entity as a state publish does, because it
    /// is the same series extended forward onto the same entity. This matters
    /// beyond what `plan` prints. The grant table is the dependency graph, so
    /// a unit whose only publish onto an entity is a forecast would otherwise
    /// produce no row. A change to that entity would then not read as a grant
    /// delta for apply to act on.
    #[test]
    fn a_forecast_publish_binds_its_entity() {
        let seer = r#"
            schema = 1

            [unit]
            name = "seer"
            kind = "automation"

            [runtime]
            command = "true"
            restart = "always"

            [entities]
            dir = "entities/seer/"

            [bus.publishes.curve]
            key = "home/forecast/global/spot_price/price/nordpool"
        "#;
        let spot_price = r#"
            schema = 1

            [entity]
            capability = "sensor"
            room = "global"

            [write_policy]
            mode = "exclusive"
            owner = "seer"
        "#;
        let house = House::from_toml(&[("seer", seer)], &[("seer", "spot_price", spot_price)]);
        let (expanded, _, _) = collect(|d| expand(&house, d));
        let (grants, warnings, errors) = collect(|d| resolve(&house, &expanded, d));
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(errors.is_empty(), "{errors:?}");
        let seer = grants
            .iter()
            .find(|g| g.unit == "seer")
            .expect("the forecast publish produces a grant row");
        assert!(!seer.is_cmd(), "a forecast row is a binding, not a writer");
        assert_eq!(
            seer.entities
                .iter()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>(),
            vec!["spot_price"],
        );
    }

    /// Two providers can forecast one aspect without conflict. Without the
    /// source in the key, the two would write one key.
    #[test]
    fn two_providers_may_forecast_one_aspect_under_their_own_sources() {
        let house = house_forecasting(
            "outdoor_temp",
            "global",
            &[
                ("smhi", "home/forecast/global/outdoor_temp/temperature/smhi"),
                ("yr", "home/forecast/global/outdoor_temp/temperature/yr"),
            ],
        );
        let (expanded, _, _) = collect(|d| expand(&house, d));
        let (_, _, errors) = collect(|d| resolve(&house, &expanded, d));
        assert!(errors.is_empty(), "{errors:?}");
    }

    /// A wildcard slot accepts anything, so a unit publishing the whole
    /// aspect space collides with one naming a single source inside it.
    #[test]
    fn a_wildcard_forecast_publish_collides_with_a_named_source() {
        let house = house_forecasting(
            "outdoor_temp",
            "global",
            &[
                ("broad", "home/forecast/global/outdoor_temp/**"),
                ("smhi", "home/forecast/global/outdoor_temp/temperature/smhi"),
            ],
        );
        let (expanded, _, _) = collect(|d| expand(&house, d));
        let (_, _, errors) = collect(|d| resolve(&house, &expanded, d));
        assert_eq!(
            errors.iter().map(|e| e.code).collect::<Vec<_>>(),
            vec![Code::ForecastPublishConflict],
        );
    }

    /// An adapter that binds sensors and publishes nothing the tests read.
    const ESPHOME: &str = r#"
        schema = 1

        [unit]
        name = "esphome"
        kind = "adapter"

        [runtime]
        command = "true"
        restart = "always"

        [entities]
        dir = "entities/esphome/"
    "#;

    /// Builds a house where `forecaster` units each publish one concrete
    /// forecast key, and `esphome` owns the entity being forecast.
    fn house_forecasting(entity_name: &str, room: &str, forecasters: &[(&str, &str)]) -> House {
        let forecasters: Vec<(&str, String)> = forecasters
            .iter()
            .map(|&(name, key)| {
                let manifest = format!(
                    r#"
                    schema = 1

                    [unit]
                    name = "{name}"
                    kind = "automation"

                    [runtime]
                    command = "true"
                    restart = "always"

                    [bus.publishes.curve]
                    key = "{key}"
                "#
                );
                (name, manifest)
            })
            .collect();
        let mut units = vec![("esphome", ESPHOME)];
        units.extend(
            forecasters
                .iter()
                .map(|(name, text)| (*name, text.as_str())),
        );
        let sensor = format!(
            r#"
            schema = 1

            [entity]
            id = "{entity_name}"
            capability = "sensor"
            room = "{room}"

            [write_policy]
            mode = "shared"
            owner = "esphome"
        "#
        );
        House::from_toml(&units, &[("esphome", entity_name, &sensor)])
    }

    /// A forecast may name an entity this unit does not bind; state may not. A
    /// forecast is a source's claim about a series' future values, and the
    /// best source is often not the binder. A weather service forecasts a
    /// sensor an adapter owns. A controller forecasts a device it commands,
    /// and because the grant graph runs from automation to device, it cannot
    /// also bind it.
    #[test]
    fn a_forecast_may_name_an_entity_another_unit_owns() {
        let house = house_forecasting(
            "outdoor_temp",
            "global",
            &[(
                "weather",
                "home/forecast/global/outdoor_temp/temperature/smhi",
            )],
        );
        let (expanded, _, _) = collect(|d| expand(&house, d));
        let (_, _, errors) = collect(|d| resolve(&house, &expanded, d));
        assert!(errors.is_empty(), "{errors:?}");
    }

    /// The entity must still exist. Dropping the ownership rule does not allow
    /// free-form keys: a forecast under a name no entity describes would be
    /// recorded but would not appear on any generated surface.
    #[test]
    fn a_forecast_for_no_entity_is_still_refused() {
        let house = house_forecasting(
            "outdoor_temp",
            "global",
            &[("weather", "home/forecast/global/nowhere/temperature/smhi")],
        );
        let (expanded, _, _) = collect(|d| expand(&house, d));
        let (_, _, errors) = collect(|d| resolve(&house, &expanded, d));
        assert_eq!(
            errors.iter().map(|e| e.code).collect::<Vec<_>>(),
            vec![Code::ForecastPublishUnbound],
        );
    }

    /// One source, one publisher. Ownership would guarantee this, since each
    /// entity has one binder, but a forecast does not require ownership. So
    /// the property is checked directly. The mirror keeps only the last
    /// document per key, so a second publisher on the same source would
    /// overwrite the first. Two providers under their own source segments are
    /// allowed; a test below covers that.
    #[test]
    fn two_units_may_not_publish_one_forecast_source() {
        let house = house_forecasting(
            "outdoor_temp",
            "global",
            &[
                (
                    "smhi",
                    "home/forecast/global/outdoor_temp/temperature/consensus",
                ),
                (
                    "yr",
                    "home/forecast/global/outdoor_temp/temperature/consensus",
                ),
            ],
        );
        let (expanded, _, _) = collect(|d| expand(&house, d));
        let (_, _, errors) = collect(|d| resolve(&house, &expanded, d));
        let conflict = errors
            .iter()
            .find(|e| e.code == Code::ForecastPublishConflict)
            .expect("two forecasters on one series must conflict");
        assert!(
            conflict.message.contains("smhi") && conflict.message.contains("yr"),
            "the conflict must name both publishers: {}",
            conflict.message
        );
    }

    /// A house where `fusion` owns a computed entity derived from one sensor
    /// an adapter owns. `subscribes` decides whether the fusion reads what it
    /// claims to.
    fn house_with_sources(source_aspect: &str, subscribes: Option<&str>) -> House {
        let subscribes = subscribes.map_or_default(|expr| format!(r#"inputs = "{expr}""#));
        let fusion = format!(
            r#"
            schema = 1

            [unit]
            name = "fusion"
            kind = "automation"

            [runtime]
            command = "true"
            restart = "always"

            [entities]
            dir = "entities/fusion/"

            [bus.subscribes]
            {subscribes}

            [bus.publishes.fused]
            key = "home/state/global/fused_temp/temperature"
        "#
        );
        let outdoor_temp = r#"
            schema = 1

            [entity]
            id = "outdoor_temp"
            capability = "sensor"
            room = "global"

            [write_policy]
            mode = "shared"
            owner = "esphome"
        "#;
        let fused_temp = format!(
            r#"
            schema = 1

            [entity]
            capability = "sensor"
            room = "global"

            [write_policy]
            mode = "shared"
            owner = "fusion"

            [sources.station]
            entity = "outdoor_temp"
            aspect = "{source_aspect}"
        "#
        );
        House::from_toml(
            &[("esphome", ESPHOME), ("fusion", &fusion)],
            &[
                ("esphome", "outdoor_temp", outdoor_temp),
                ("fusion", "fused_temp", &fused_temp),
            ],
        )
    }

    /// A declared source resolves to the contributor's own state key. The
    /// entity keeps publishing where it did, and the declaration only refers
    /// to it. It is not a second place for the value.
    #[test]
    fn a_declared_source_resolves_to_the_contributors_key() {
        let house = house_with_sources(
            "temperature",
            Some("home/state/global/outdoor_temp/temperature"),
        );
        let (expanded, _, _) = collect(|d| expand(&house, d));
        let (sources, warnings, errors) = collect(|d| resolve_sources(&house, &expanded, d));
        assert!(errors.is_empty(), "{errors:?}");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            sources,
            vec![Source {
                entity: "fused_temp".to_string(),
                name: "station".to_string(),
                contributor: Contributor {
                    entity: "outdoor_temp".to_string(),
                    aspect: "temperature".to_string(),
                    owner: "esphome".to_string(),
                    key: "home/state/global/outdoor_temp/temperature".to_string(),
                },
                note: None,
            }]
        );
    }

    /// The declaration is checked. A unit that claims to derive from a reading
    /// it does not subscribe is either mis-declared or has dead code, and the
    /// overlay would draw a contributor line that plays no part.
    #[test]
    fn a_source_the_owner_does_not_subscribe_warns() {
        let house = house_with_sources("temperature", None);
        let (expanded, _, _) = collect(|d| expand(&house, d));
        let (sources, warnings, errors) = collect(|d| resolve_sources(&house, &expanded, d));
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(sources.len(), 1, "the source still resolves");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].message.contains("does not subscribe"),
            "{}",
            warnings[0]
        );
    }

    /// A contributor that does not exist is an error, not a warning: the
    /// overlay has no series to draw and the declaration names nothing.
    #[test]
    fn a_source_naming_no_entity_is_refused() {
        let mut house = house_with_sources("temperature", None);
        let fused = house
            .entities
            .iter_mut()
            .find(|e| e.name == "fused_temp")
            .unwrap();
        fused
            .file
            .sources
            .as_mut()
            .unwrap()
            .get_mut("station")
            .unwrap()
            .entity = "ghost".to_string();
        let (expanded, _, _) = collect(|d| expand(&house, d));
        let (_, _, errors) = collect(|d| resolve_sources(&house, &expanded, d));
        assert_eq!(
            errors.iter().map(|e| e.code).collect::<Vec<_>>(),
            vec![Code::SourceUnknownEntity],
        );
    }

    /// Every bound entity sits in its owner's state row. So a change to an
    /// entity nobody is granted onto, the lamp here, is still a grant delta
    /// (docs/design.md#the-grant-table: entity moves and write-policy changes
    /// are structural).
    #[test]
    fn a_change_to_an_ungranted_entity_changes_the_grant_table() {
        let zigbee = [
            ZIGBEE,
            r#"
            [bus.publishes.state]
            key = "home/state/{room}/{entity}/**"
            "#,
        ]
        .concat();
        let with_state = || {
            House::from_toml(
                &[("zigbee", &zigbee), ("night_mode", NIGHT_MODE)],
                LOCK_ENTITIES,
            )
        };
        let baseline = with_state();
        let (expanded, _, _) = collect(|d| expand(&baseline, d));
        let (grants, _, _) = collect(|d| resolve(&baseline, &expanded, d));
        let zigbee = grants.iter().find(|g| g.unit == "zigbee").unwrap();
        assert!(!zigbee.is_cmd());
        assert_eq!(
            zigbee
                .entities
                .iter()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>(),
            ["lamp", "lock"]
        );

        let mut moved = with_state();
        moved.entities[0].file.entity.room = "porch".to_string();
        let (expanded, _, _) = collect(|d| expand(&moved, d));
        let (moved_grants, _, _) = collect(|d| resolve(&moved, &expanded, d));
        assert_ne!(
            grants, moved_grants,
            "moving the ungranted lamp must change the table"
        );

        let mut retyped = with_state();
        retyped.entities[0].file.entity.capability = "switch".to_string();
        let (expanded, _, _) = collect(|d| expand(&retyped, d));
        let (retyped_grants, _, _) = collect(|d| resolve(&retyped, &expanded, d));
        assert_ne!(
            grants, retyped_grants,
            "a capability change must change the table"
        );
    }

    /// Exclusivity counts writers per unit: two bindings of one unit onto
    /// one exclusive entity are one writer.
    #[test]
    fn two_bindings_in_one_unit_are_one_writer() {
        let night_mode = [
            NIGHT_MODE,
            r#"
            [bus.publishes.lamp_on]
            key = "home/cmd/kitchen/lamp/on"
            capability = "light"
            priority = "automation"

            [bus.publishes.lamp_brightness]
            key = "home/cmd/kitchen/lamp/brightness"
            capability = "light"
            priority = "automation"
            "#,
        ]
        .concat();
        let mut house = House::from_toml(
            &[("zigbee", ZIGBEE), ("night_mode", &night_mode)],
            LOCK_ENTITIES,
        );
        house.entities[0].file.write_policy.mode = Some(WriteMode::Exclusive);
        let (expanded, _, _) = collect(|d| expand(&house, d));
        let (_, _, errors) = collect(|d| resolve(&house, &expanded, d));
        assert!(
            !errors
                .iter()
                .any(|e| e.code == Code::ExclusiveWriteConflict),
            "{errors:?}"
        );
    }

    /// The bus of `house_with_lock`'s `night_mode` automation.
    fn night_mode_bus(house: &mut House) -> &mut crate::manifest::BusSection {
        house
            .units
            .iter_mut()
            .find(|u| u.manifest.unit.name == "night_mode")
            .and_then(|u| u.manifest.bus.as_mut())
            .expect("night_mode has a bus")
    }

    /// The manual band belongs to the family's surfaces. An automation that
    /// claims it still plans, with a warning.
    #[test]
    fn manual_band_on_an_automation_is_a_plan_warning() {
        let mut house = house_with_lock(false);
        let bus = night_mode_bus(&mut house);
        bus.publishes.get_mut("lock").unwrap().priority = Some(Priority::Manual);
        let (expanded, _, _) = collect(|d| expand(&house, d));
        let (_, warnings, _) = collect(|d| resolve(&house, &expanded, d));
        assert!(
            warnings.iter().any(|w| {
                w.to_string().starts_with(
                "night_mode.lock: publish declares priority \"manual\" on automation \"night_mode\""
            )
            }),
            "{warnings:?}"
        );
    }

    #[test]
    fn arbitrated_entity_with_no_arbiter_publish_is_a_plan_error() {
        let house = house_with_lock(false);
        let (expanded, _warnings, expand_errors) = collect(|d| expand(&house, d));
        assert!(expand_errors.is_empty(), "{expand_errors:?}");

        let (_grants, _warnings, errors) = collect(|d| resolve(&house, &expanded, d));
        assert!(
            errors
                .iter()
                .any(|e| e.code == Code::ArbitratedUncovered && e.subject == "lock"),
            "{errors:?}"
        );
    }

    /// A house with an arbitrated burner and a shared lamp, an arbiter, and
    /// automations holding at `band` under the given keys.
    fn house_holding(holders: &[(&str, &str, Priority)]) -> House {
        let holders: Vec<(&str, String)> = holders
            .iter()
            .map(|&(name, key, band)| (name, holder(name, key, band)))
            .collect();
        house_with_holders(&holders)
    }

    /// The manifest of automation `name` holding under `key` at `band`. It
    /// ends in the hold's own table, so a test may append to it.
    fn holder(name: &str, key: &str, band: Priority) -> String {
        let capability = if key.contains("lamp") {
            "light"
        } else if key.contains("heat_pump") {
            "climate"
        } else {
            "burner"
        };
        format!(
            r#"
            schema = 1

            [unit]
            name = "{name}"
            kind = "automation"

            [runtime]
            command = "true"
            restart = "always"

            [bus.publishes.hold]
            key = "{key}"
            capability = "{capability}"
            priority = "{band}"
            "#
        )
    }

    /// The house of [`house_holding`], with each holder given as its
    /// (name, manifest).
    fn house_with_holders(holders: &[(&str, String)]) -> House {
        let arbiter = r#"
            schema = 1

            [unit]
            name = "arbiter"
            kind = "service"

            [runtime]
            command = "true"
            restart = "always"

            [bus.publishes.arbiter]
            key = "home/arbiter/**"

            [bus.publishes.holds]
            key = "home/hold/arbiter/**"
        "#;
        let aduro = r#"
            schema = 1

            [unit]
            name = "aduro"
            kind = "adapter"

            [runtime]
            command = "true"
            restart = "always"

            [entities]
            dir = "entities/aduro/"
        "#;
        let stove = r#"
            schema = 1

            [entity]
            id = "stove"
            capability = "burner"
            room = "utility"

            [write_policy]
            mode = "arbitrated"
            owner = "aduro"
        "#;
        let lamp = r#"
            schema = 1

            [entity]
            id = "lamp"
            capability = "light"
            room = "kitchen"

            [write_policy]
            mode = "shared"
            owner = "aduro"
        "#;
        let heat_pump = r#"
            schema = 1

            [entity]
            id = "heat_pump"
            capability = "climate"
            room = "utility"

            [write_policy]
            mode = "arbitrated"
            owner = "aduro"
        "#;
        let mut units = vec![("arbiter", arbiter)];
        units.extend(holders.iter().map(|(name, text)| (*name, text.as_str())));
        units.push(("aduro", aduro));
        House::from_toml(
            &units,
            &[
                ("aduro", "stove", stove),
                ("aduro", "lamp", lamp),
                ("aduro", "heat_pump", heat_pump),
            ],
        )
    }

    fn resolve_holding(
        holders: &[(&str, &str, Priority)],
    ) -> (Vec<Grant>, Vec<Warning>, Vec<ValidationError>) {
        let house = house_holding(holders);
        let (expanded, _warnings, expand_errors) = collect(|d| expand(&house, d));
        assert!(expand_errors.is_empty(), "{expand_errors:?}");
        collect(|d| resolve(&house, &expanded, d))
    }

    #[test]
    fn a_hold_publish_is_a_holder_row() {
        let (grants, warnings, errors) = resolve_holding(&[(
            "heating",
            "home/hold/heating/utility/stove/on",
            Priority::Automation,
        )]);
        assert!(errors.is_empty(), "{errors:?}");
        assert!(warnings.is_empty(), "{warnings:?}");
        let row = grants
            .iter()
            .find(|g| g.unit == "heating")
            .expect("a holder row");
        assert!(row.hold && !row.is_cmd(), "a holder is not a writer");
        assert_eq!(row.priority, Some(Priority::Automation));
        assert_eq!(row.entities[0].name, "stove");
        assert!(
            !grants.iter().any(|g| g.unit == "arbiter" && g.hold),
            "the arbiter's own family holds grant nothing"
        );
    }

    #[test]
    fn a_hold_on_an_entity_nobody_arbitrates_is_refused() {
        let (_grants, _warnings, errors) = resolve_holding(&[(
            "lights",
            "home/hold/lights/kitchen/lamp/on",
            Priority::Automation,
        )]);
        assert!(
            errors.iter().any(|e| e.code == Code::HoldNotArbitrated),
            "{errors:?}"
        );
    }

    #[test]
    fn a_hold_at_the_family_bands_is_refused() {
        for band in [Priority::Family, Priority::Manual] {
            let (_grants, _warnings, errors) =
                resolve_holding(&[("heating", "home/hold/heating/utility/stove/on", band)]);
            assert!(
                errors.iter().any(|e| e.code == Code::HoldBand),
                "{band}: {errors:?}"
            );
        }
    }

    #[test]
    fn two_holders_of_the_rest_aspect_do_not_warn() {
        // The burner's `on` has a rest value, so the two are told apart by
        // it, even where one key's wildcard reaches beyond it.
        let (_grants, warnings, errors) = resolve_holding(&[
            (
                "flue_guard",
                "home/hold/flue_guard/utility/stove/on",
                Priority::Automation,
            ),
            (
                "heating",
                "home/hold/heating/utility/stove/**",
                Priority::Automation,
            ),
        ]);
        assert!(errors.is_empty(), "{errors:?}");
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn two_holders_of_an_aspect_without_a_rest_value_warn() {
        // Both reach the burner's power_level, which has none.
        let (_grants, warnings, _errors) = resolve_holding(&[
            (
                "flue_guard",
                "home/hold/flue_guard/utility/stove/**",
                Priority::Automation,
            ),
            (
                "heating",
                "home/hold/heating/utility/stove/power_level",
                Priority::Automation,
            ),
        ]);
        assert!(
            warnings.iter().any(|w| w
                .to_string()
                .starts_with("stove: flue_guard and heating hold it")),
            "{warnings:?}"
        );
    }

    #[test]
    fn two_holders_of_different_aspects_do_not_warn() {
        let (_grants, warnings, _errors) = resolve_holding(&[
            (
                "curve",
                "home/hold/curve/utility/heat_pump/setpoint",
                Priority::Automation,
            ),
            (
                "price",
                "home/hold/price/utility/heat_pump/offset",
                Priority::Automation,
            ),
        ]);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn two_holders_at_one_band_without_a_rest_value_warn() {
        let (_grants, warnings, errors) = resolve_holding(&[
            (
                "curve",
                "home/hold/curve/utility/heat_pump/setpoint",
                Priority::Automation,
            ),
            (
                "setback",
                "home/hold/setback/utility/heat_pump/setpoint",
                Priority::Automation,
            ),
        ]);
        assert!(errors.is_empty(), "{errors:?}");
        assert!(
            warnings.iter().any(|w| w
                .to_string()
                .starts_with("heat_pump: curve and setback hold it at automation")),
            "{warnings:?}"
        );
    }

    #[test]
    fn a_hold_under_another_units_name_is_refused() {
        let (_grants, _warnings, errors) = resolve_holding(&[(
            "heating",
            "home/hold/arbiter/utility/stove/on",
            Priority::Automation,
        )]);
        assert!(
            errors.iter().any(|e| e.code == Code::ReservedClassPublish),
            "{errors:?}"
        );
    }

    #[test]
    fn on_loss_rides_the_holder_row_and_only_a_hold_takes_it() {
        let flue_guard = holder(
            "flue_guard",
            "home/hold/flue_guard/utility/stove/on",
            Priority::Automation,
        ) + r#"
            on_loss = "keep"

            [bus.publishes.state]
            key = "home/state/utility/stove/flue"
            on_loss = "release"
        "#;
        let house = house_with_holders(&[("flue_guard", flue_guard)]);
        let (expanded, _warnings, _expand_errors) = collect(|d| expand(&house, d));
        let (grants, _warnings, errors) = collect(|d| resolve(&house, &expanded, d));
        let row = grants.iter().find(|g| g.hold).expect("a holder row");
        assert_eq!(row.on_loss, Some(OnLoss::Keep));
        assert!(
            errors
                .iter()
                .any(|e| e.code == Code::OnLossNotHold && e.subject == "flue_guard.state"),
            "{errors:?}"
        );
    }
}
