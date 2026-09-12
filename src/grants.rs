use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::error::ValidationError;
use crate::expand::{Direction, ExpandedKey};
use crate::keyspace::Segment;
use crate::manifest::{Priority, UnitKind, WriteMode, CAPABILITIES};
use crate::repo::House;

/// One resolved grant: a publish expression resolved against the concrete
/// entity set. A cmd-class row is a writer (a non-adapter's cmd publish,
/// with its capability and band); a state-class row is a binding (a
/// binding unit's state publish over the entities it embodies). The table
/// doubles as the dependency graph (unit -> entities -> owner units).
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
    /// The resolved key expressions, sorted. Part of the grant's identity:
    /// widening `.../on` to `.../**` is a grant delta even when the same
    /// entities match.
    #[serde(default)]
    pub keys: Vec<String>,
    /// Granted entities (key match + capability match), sorted by name.
    pub entities: Vec<GrantEntity>,
}

impl Grant {
    /// A cmd-class row: a writer. State rows record bindings only.
    pub fn is_cmd(&self) -> bool {
        self.capability.is_some()
    }
}

/// A granted entity with the policy facts the grant table is the record
/// of. Because these live in the table — and every bound entity sits in
/// its owner's state row — an entity move, a write-mode flip, a
/// capability change or a re-binding IS a grant-table delta, and any grant
/// delta escalates the plan to structural (docs/design.md, Plan/apply
/// mechanics), with the owner shown exactly what changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantEntity {
    pub name: String,
    pub room: String,
    #[serde(default)]
    pub capability: String,
    pub write: WriteMode,
    /// The binding unit — the walk-order edge source. An adapter, or an
    /// automation for a commandable virtual entity.
    pub owner: String,
}

/// One resolved feed: a device input wired to a source aspect
/// (docs/design.md, Device feeds). Rendered in the plan next to the grant
/// table; not a walk-order edge — a control loop that reads a device and
/// feeds a term back is legitimately cyclic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Feed {
    /// The fed entity and its adapter's input name.
    pub entity: String,
    pub input: String,
    /// The source, resolved.
    pub source_entity: String,
    pub source_aspect: String,
    pub source_owner: String,
    pub key: String,
}

/// Resolves every `[inputs]` block: the source exists, the target is a
/// device, and an automation-owned source actually publishes the aspect.
pub fn resolve_feeds(house: &House, expanded: &[ExpandedKey]) -> (Vec<Feed>, Vec<ValidationError>) {
    let mut feeds = Vec::new();
    let mut errors = Vec::new();
    for entity in &house.entities {
        let Some(inputs) = &entity.file.inputs else {
            continue;
        };
        let file = Some(entity.path.clone());
        let owner_is_automation = house
            .unit(&entity.owner)
            .map(|u| u.manifest.unit.kind == UnitKind::Automation)
            .unwrap_or(false);
        if owner_is_automation {
            errors.push(ValidationError::new(
                "virtual-entity-fed",
                &entity.name,
                "automation-owned entities have no device inputs to feed",
                file.clone(),
            ));
            continue;
        }
        for (input, source) in inputs {
            let subject = format!("{}.{input}", entity.name);
            let Some(src) = house.entities.iter().find(|e| e.name == source.entity) else {
                errors.push(ValidationError::new(
                    "input-unknown-entity",
                    subject,
                    format!("source entity \"{}\" does not exist", source.entity),
                    file.clone(),
                ));
                continue;
            };
            let key = format!(
                "home/state/{}/{}/{}",
                src.file.entity.room, src.name, source.aspect
            );
            let src_is_automation = house
                .unit(&src.owner)
                .map(|u| u.manifest.unit.kind == UnitKind::Automation)
                .unwrap_or(false);
            if src_is_automation {
                let published = expanded.iter().any(|k| {
                    k.unit == src.owner
                        && k.direction == Direction::Publishes
                        && k.exprs
                            .iter()
                            .any(|e| e.matches_prefix(&key.split('/').collect::<Vec<_>>()))
                });
                if !published {
                    errors.push(ValidationError::new(
                        "input-unpublished-aspect",
                        subject,
                        format!(
                            "\"{}\" does not publish {key}; a fed aspect must be in its owner's [bus.publishes]",
                            src.owner
                        ),
                        file.clone(),
                    ));
                    continue;
                }
            }
            feeds.push(Feed {
                entity: entity.name.clone(),
                input: input.clone(),
                source_entity: src.name.clone(),
                source_aspect: source.aspect.clone(),
                source_owner: src.owner.clone(),
                key,
            });
        }
    }
    (feeds, errors)
}

/// Resolves the grant table and enforces write policy.
pub fn resolve(
    house: &House,
    expanded: &[ExpandedKey],
) -> (Vec<Grant>, Vec<String>, Vec<ValidationError>) {
    let mut grants = Vec::new();
    let mut warnings = Vec::new();
    let mut errors = Vec::new();

    for key in expanded {
        if key.direction != Direction::Publishes {
            continue;
        }
        // Classify by the source: a templated publish that expanded to
        // nothing has no exprs to betray its class, and it still gets the
        // capability checks and the "matches no entities" warning.
        let class = key.source.split('/').nth(1).unwrap_or_default();
        let unit = house
            .unit(&key.unit)
            .expect("expanded key from loaded unit");
        let spec = &unit.manifest.bus.as_ref().expect("unit has bus").publishes[&key.entry];
        let subject = format!("{}.{}", key.unit, key.entry);
        let mut keys: Vec<String> = key.exprs.iter().map(ToString::to_string).collect();
        keys.sort();

        // A binding unit's state publish: the record of what it embodies,
        // so a change to any bound entity is a grant delta.
        if class == "state" {
            let mut bound: Vec<GrantEntity> = house
                .entities
                .iter()
                .filter(|e| e.owner == key.unit)
                .filter(|e| {
                    let prefix = [
                        "home",
                        "state",
                        e.file.entity.room.as_str(),
                        e.name.as_str(),
                    ];
                    key.exprs.iter().any(|expr| expr.matches_prefix(&prefix))
                })
                .map(|e| GrantEntity {
                    name: e.name.clone(),
                    room: e.file.entity.room.clone(),
                    capability: e.file.entity.capability.clone(),
                    write: e.file.write_policy.mode,
                    owner: e.owner.clone(),
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
            });
            continue;
        }
        if class != "cmd" || key.kind == UnitKind::Adapter {
            continue;
        }

        let Some(capability) = spec.capability.clone() else {
            errors.push(ValidationError::new(
                "publish-missing-capability",
                subject,
                format!("cmd publish \"{}\" must declare a capability", key.source),
                Some(unit.path.clone()),
            ));
            continue;
        };
        if !CAPABILITIES.contains(&capability.as_str()) {
            errors.push(ValidationError::new(
                "unknown-capability",
                subject,
                format!("unknown capability \"{capability}\""),
                Some(unit.path.clone()),
            ));
            continue;
        }

        let mut granted: Vec<GrantEntity> = house
            .entities
            .iter()
            .filter(|e| e.file.entity.capability == capability)
            .filter(|e| {
                let prefix = ["home", "cmd", e.file.entity.room.as_str(), e.name.as_str()];
                key.exprs.iter().any(|expr| expr.matches_prefix(&prefix))
            })
            .map(|e| GrantEntity {
                name: e.name.clone(),
                room: e.file.entity.room.clone(),
                capability: capability.clone(),
                write: e.file.write_policy.mode,
                owner: e.owner.clone(),
            })
            .collect();
        granted.sort_by(|a, b| a.name.cmp(&b.name));
        granted.dedup();

        if granted.is_empty() {
            warnings.push(format!("publish {subject} matches no entities"));
        }
        // The manual band is the family's (dashboard, voice) and is exempt
        // from exclusive-write checks; an automation claiming it is worth a
        // look in the plan, though nothing forbids it.
        let priority = spec.priority.unwrap_or(Priority::Automation);
        if priority == Priority::Manual && unit.manifest.unit.kind != UnitKind::Service {
            warnings.push(format!(
                "publish {subject} declares priority \"manual\" on {} \"{}\"; the manual band is the family's and is exempt from exclusive-write checks",
                unit.manifest.unit.kind, key.unit
            ));
        }
        grants.push(Grant {
            unit: key.unit.clone(),
            publish: key.entry.clone(),
            capability: Some(capability),
            priority: Some(priority),
            keys,
            entities: granted,
        });
    }

    grants.sort_by(|a, b| (&a.unit, &a.publish).cmp(&(&b.unit, &b.publish)));

    // Write-policy enforcement: two writers on an exclusive entity is an error.
    // Exclusivity constrains the automation band only; manual-band units
    // (dashboard, voice) sit above it by construction and never count. A
    // writer is a unit, not a binding: authority is per process.
    let mut writers: BTreeMap<&str, BTreeMap<&str, Vec<String>>> = BTreeMap::new();
    for grant in &grants {
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
        if entity.file.write_policy.mode != WriteMode::Exclusive {
            continue;
        }
        if let Some(writers) = writers.get(entity.name.as_str()) {
            if writers.len() > 1 {
                let bindings: Vec<&str> = writers.values().flatten().map(String::as_str).collect();
                errors.push(ValidationError::new(
                    "exclusive-write-conflict",
                    &entity.name,
                    format!(
                        "exclusive entity has {} writers: {}",
                        writers.len(),
                        bindings.join(", ")
                    ),
                    Some(entity.path.clone()),
                ));
            }
        }
    }

    // Arbitration coverage: an arbitrated entity with no arbiter-class
    // publish reaching it would silently never receive a write token — the
    // arbiter service has no path to it. Mirrors the exclusive-write-conflict
    // check above but over `expanded` directly, since arbiter-class publishes
    // never form cmd-class grants.
    for entity in &house.entities {
        if entity.file.write_policy.mode != WriteMode::Arbitrated {
            continue;
        }
        let prefix = [
            "home",
            "arbiter",
            entity.file.entity.room.as_str(),
            entity.name.as_str(),
        ];
        let covered = expanded.iter().any(|k| {
            k.direction == Direction::Publishes
                && k.exprs
                    .iter()
                    .any(|e| e.class() == Some("arbiter") && e.matches_prefix(&prefix))
        });
        if !covered {
            errors.push(ValidationError::new(
                "arbitrated-uncovered",
                &entity.name,
                format!(
                    "arbitrated entity \"{}\" has no arbiter-class publish covering it",
                    entity.name
                ),
                Some(entity.path.clone()),
            ));
        }
    }

    // A commandable virtual entity is a latch (docs/design.md, Commandable
    // virtual entities): its owning automation subscribes to the entity's
    // cmd keys and sets its own state. A cmd-class grant onto an
    // automation-owned entity nobody subscribes for would hand commands to
    // a producer that never receives them, so it stays refused.
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
            let listens = expanded.iter().any(|k| {
                k.unit == entity.owner
                    && k.direction == Direction::Subscribes
                    && k.exprs
                        .iter()
                        .any(|e| e.class() == Some("cmd") && e.matches_prefix(&prefix))
            });
            if !listens {
                errors.push(ValidationError::new(
                    "virtual-entity-commanded",
                    name,
                    format!(
                        "\"{name}\" is bound by automation \"{}\", which subscribes to no home/cmd/{room}/{name} keys (cmd publish {}.{})",
                        entity.owner, grant.unit, grant.publish
                    ),
                    Some(entity.path.clone()),
                ));
            }
        }
    }

    // Grant edges run owner -> granting unit, and with automations as owners
    // a cycle is possible: A commands an entity B binds while B commands one
    // A binds. The apply walk needs an order, so refuse the house at plan
    // time rather than start units in a silently arbitrary one.
    let edges: BTreeSet<(&str, &str)> = grants
        .iter()
        .flat_map(|g| {
            g.entities
                .iter()
                .map(move |e| (e.owner.as_str(), g.unit.as_str()))
        })
        .filter(|(owner, unit)| owner != unit)
        .collect();
    let mut remaining: BTreeSet<&str> = edges.iter().flat_map(|(a, d)| [*a, *d]).collect();
    loop {
        let free: Vec<&str> = remaining
            .iter()
            .filter(|u| !edges.iter().any(|(a, d)| d == *u && remaining.contains(a)))
            .copied()
            .collect();
        if free.is_empty() {
            break;
        }
        for unit in free {
            remaining.remove(unit);
        }
    }
    if !remaining.is_empty() {
        let members: Vec<&str> = remaining.into_iter().collect();
        errors.push(ValidationError::new(
            "grant-cycle",
            members.join(", "),
            "each of these units commands an entity another of them binds, so no apply order exists",
            None,
        ));
    }

    // State keys belong to bound entities: templated state publishes are
    // bound by construction; a concrete one must name a bound entity's room
    // and name literally. Closes the free-form-state-key hole that virtual
    // sensors would otherwise ride through.
    for key in expanded {
        if key.direction != Direction::Publishes || key.templated {
            continue;
        }
        for expr in &key.exprs {
            if expr.class() != Some("state") {
                continue;
            }
            let unit = house
                .unit(&key.unit)
                .expect("expanded key from loaded unit");
            let subject = format!("{}.{}", key.unit, key.entry);
            let (room, entity) = (expr.0.get(2), expr.0.get(3));
            let (Some(Segment::Literal(room)), Some(Segment::Literal(entity))) = (room, entity)
            else {
                errors.push(ValidationError::new(
                    "state-publish-unbound",
                    subject,
                    format!(
                        "state publish \"{}\" needs literal room and entity segments (or {{room}}/{{entity}} templates)",
                        key.source
                    ),
                    Some(unit.path.clone()),
                ));
                continue;
            };
            let bound = house
                .entities
                .iter()
                .any(|e| &e.name == entity && e.owner == key.unit && &e.file.entity.room == room);
            if !bound {
                errors.push(ValidationError::new(
                    "state-publish-unbound",
                    subject,
                    format!(
                        "state key \"home/state/{room}/{entity}/…\" is not under an entity bound by \"{}\"",
                        key.unit
                    ),
                    Some(unit.path.clone()),
                ));
            }
        }
    }

    // Reserved classes (docs/design.md, Key space): `config` and `meta` are
    // the core's alone; `health` and `discovery` are per unit, under the
    // publishing unit's own name; `arbiter`, `clock` and `history` are one
    // service's output each. The SDK only checks that a published key is
    // within a declared expression, so without this an automation could
    // declare `home/arbiter/**` and forge post-arbitration commands, or
    // another unit's discovery record, with an empty grant table.
    let publish_class = |key: &ExpandedKey| -> Option<(String, Option<String>)> {
        let mut segments = key.source.split('/').skip(1).map(str::to_string);
        Some((segments.next()?, segments.next()))
    };
    let mut singleton_publishers: BTreeMap<String, BTreeSet<&str>> = BTreeMap::new();
    for key in expanded
        .iter()
        .filter(|k| k.direction == Direction::Publishes)
    {
        if let Some((class, _)) = publish_class(key) {
            if matches!(class.as_str(), "arbiter" | "clock" | "history") {
                singleton_publishers
                    .entry(class)
                    .or_default()
                    .insert(key.unit.as_str());
            }
        }
    }
    for key in expanded
        .iter()
        .filter(|k| k.direction == Direction::Publishes)
    {
        let Some((class, next)) = publish_class(key) else {
            continue;
        };
        let unit = house
            .unit(&key.unit)
            .expect("expanded key from loaded unit");
        let message = match class.as_str() {
            "config" | "meta" => Some(format!(
                "\"{}\" publishes under home/{class}/, which only the core writes",
                key.source
            )),
            "health" | "discovery" if next.as_deref() != Some(key.unit.as_str()) => Some(format!(
                "\"{}\" must sit under this unit's own name: home/{class}/{}/...",
                key.source, key.unit
            )),
            "arbiter" | "clock" | "history" => {
                let publishers = &singleton_publishers[&class];
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
            errors.push(ValidationError::new(
                "reserved-class-publish",
                format!("{}.{}", key.unit, key.entry),
                message,
                Some(unit.path.clone()),
            ));
        }
    }

    (grants, warnings, errors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expand::expand;
    use crate::keyspace::KeyExpr;
    use crate::manifest::{
        BusSection, EntityFile, EntitySection, PublishSpec, RestartPolicy, RuntimeSection,
        UnitManifest, UnitSection, WritePolicy,
    };
    use crate::repo::{LoadedEntity, LoadedUnit};

    fn unit(name: &str, kind: UnitKind, bus: BusSection) -> LoadedUnit {
        LoadedUnit {
            manifest: UnitManifest {
                schema: 1,
                unit: UnitSection {
                    name: name.to_string(),
                    kind,
                    description: None,
                    inputs: None,
                },
                runtime: RuntimeSection {
                    command: "true".to_string(),
                    restart: RestartPolicy::Always,
                    shutdown_grace_s: None,
                    env: None,
                },
                discovery: None,
                bus: Some(bus),
                params: None,
                entities: None,
                naming: None,
            },
            path: format!("units/{name}.toml"),
        }
    }

    fn entity(
        name: &str,
        room: &str,
        capability: &str,
        mode: WriteMode,
        adapter: &str,
    ) -> LoadedEntity {
        LoadedEntity {
            name: name.to_string(),
            file: EntityFile {
                schema: 1,
                entity: EntitySection {
                    id: name.to_string(),
                    capability: capability.to_string(),
                    features: vec![],
                    room: room.to_string(),
                },
                naming: None,
                write_policy: WritePolicy {
                    mode,
                    owner: adapter.to_string(),
                },
                inputs: None,
                dashboard: None,
            },
            path: format!("entities/{adapter}/{name}.toml"),
            owner: adapter.to_string(),
        }
    }

    /// Builds a house with an arbitrated lock and a shared lamp, both bound
    /// to adapter "zigbee", plus an automation wishing to command the lock.
    /// `arbiter_bus` lets each test decide whether an arbiter-class publish
    /// covers the lock.
    fn house_with_lock(arbiter_bus: Option<BusSection>) -> House {
        let mut zigbee_subscribes = BTreeMap::new();
        zigbee_subscribes.insert(
            "commands".to_string(),
            "home/cmd/{room}/{entity}/**".to_string(),
        );
        zigbee_subscribes.insert(
            "arbiter_commands".to_string(),
            "home/arbiter/{room}/{entity}/**".to_string(),
        );
        let zigbee_bus = BusSection {
            subscribes: zigbee_subscribes,
            publishes: BTreeMap::new(),
        };

        let mut night_mode_publishes = BTreeMap::new();
        night_mode_publishes.insert(
            "lock".to_string(),
            PublishSpec {
                key: "home/cmd/hallway/lock/lock".to_string(),
                capability: Some("lock".to_string()),
                priority: Some(Priority::Automation),
            },
        );
        let night_mode_bus = BusSection {
            subscribes: BTreeMap::new(),
            publishes: night_mode_publishes,
        };

        let mut units = vec![
            unit("zigbee", UnitKind::Adapter, zigbee_bus),
            unit("night_mode", UnitKind::Automation, night_mode_bus),
        ];
        if let Some(arbiter_bus) = arbiter_bus {
            units.push(unit("arbiter", UnitKind::Service, arbiter_bus));
        }

        House {
            units,
            entities: vec![
                entity("lamp", "kitchen", "light", WriteMode::Shared, "zigbee"),
                entity("lock", "hallway", "lock", WriteMode::Arbitrated, "zigbee"),
            ],
            zones: BTreeMap::new(),
        }
    }

    #[test]
    fn arbitrated_entity_covered_by_arbiter_publish_has_no_errors() {
        let mut publishes = BTreeMap::new();
        // A service (not an adapter) cannot use {room}/{entity} templates, so
        // it covers arbitrated entities with a plain wildcard instead.
        publishes.insert(
            "forwarded".to_string(),
            PublishSpec {
                key: "home/arbiter/**".to_string(),
                capability: None,
                priority: None,
            },
        );
        let arbiter_bus = BusSection {
            subscribes: BTreeMap::new(),
            publishes,
        };
        let house = house_with_lock(Some(arbiter_bus));

        let (expanded, _warnings, expand_errors) = expand(&house);
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

        let (grants, warnings, errors) = resolve(&house, &expanded);
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
            }]
        );
    }

    /// The tier-escalation property behind docs/design.md's "entity moves,
    /// write-policy changes" structural rule: a room move or a write-mode
    /// flip changes the resolved grant table, so it diffs as a grant delta.
    #[test]
    fn entity_move_and_policy_flip_change_the_grant_table() {
        let arbiter_bus = || {
            let mut publishes = BTreeMap::new();
            publishes.insert(
                "forwarded".to_string(),
                PublishSpec {
                    key: "home/arbiter/**".to_string(),
                    capability: None,
                    priority: None,
                },
            );
            Some(BusSection {
                subscribes: BTreeMap::new(),
                publishes,
            })
        };
        let baseline = house_with_lock(arbiter_bus());
        let (expanded, _, _) = expand(&baseline);
        let (grants, _, _) = resolve(&baseline, &expanded);

        let mut moved = house_with_lock(arbiter_bus());
        moved.entities[1].file.entity.room = "porch".to_string();
        // The publish key still names the old room, so re-expansion changes
        // the granted set — either way the tables differ.
        let (expanded, _, _) = expand(&moved);
        let (moved_grants, _, _) = resolve(&moved, &expanded);
        assert_ne!(
            grants, moved_grants,
            "a room move must change the grant table"
        );

        let mut flipped = house_with_lock(arbiter_bus());
        flipped.entities[0].file.write_policy.mode = WriteMode::Exclusive;
        // The lamp is granted to nobody; flip the lock instead, which
        // night_mode writes.
        flipped.entities[1].file.write_policy.mode = WriteMode::Shared;
        let (expanded, _, _) = expand(&flipped);
        let (flipped_grants, _, _) = resolve(&flipped, &expanded);
        assert_ne!(
            grants, flipped_grants,
            "a write-mode flip must change the grant table"
        );
    }

    /// A latch: automation "modes" binds a switch, subscribes to its cmd
    /// keys and publishes its state; automation "buttons" commands it.
    /// `listens` decides whether modes declares the cmd subscription.
    fn house_with_latch(listens: bool) -> House {
        let mut modes_subscribes = BTreeMap::new();
        if listens {
            modes_subscribes.insert(
                "commands".to_string(),
                "home/cmd/{room}/{entity}/**".to_string(),
            );
        }
        let mut modes_publishes = BTreeMap::new();
        modes_publishes.insert(
            "state".to_string(),
            PublishSpec {
                key: "home/state/{room}/{entity}/**".to_string(),
                capability: None,
                priority: None,
            },
        );
        let modes_bus = BusSection {
            subscribes: modes_subscribes,
            publishes: modes_publishes,
        };

        let mut buttons_publishes = BTreeMap::new();
        buttons_publishes.insert(
            "mode".to_string(),
            PublishSpec {
                key: "home/cmd/global/house_mode/on".to_string(),
                capability: Some("switch".to_string()),
                priority: Some(Priority::Automation),
            },
        );
        let buttons_bus = BusSection {
            subscribes: BTreeMap::new(),
            publishes: buttons_publishes,
        };

        House {
            units: vec![
                unit("modes", UnitKind::Automation, modes_bus),
                unit("buttons", UnitKind::Automation, buttons_bus),
            ],
            entities: vec![entity(
                "house_mode",
                "global",
                "switch",
                WriteMode::Shared,
                "modes",
            )],
            zones: BTreeMap::new(),
        }
    }

    #[test]
    fn a_commanded_virtual_entity_whose_owner_listens_is_a_latch() {
        let house = house_with_latch(true);
        let (expanded, _warnings, expand_errors) = expand(&house);
        assert!(expand_errors.is_empty(), "{expand_errors:?}");

        let (grants, warnings, errors) = resolve(&house, &expanded);
        assert!(errors.is_empty(), "{errors:?}");
        assert!(warnings.is_empty(), "{warnings:?}");
        let buttons = grants.iter().find(|g| g.unit == "buttons").unwrap();
        // The owner is the automation: it becomes the walk-order edge source.
        assert_eq!(
            buttons.entities,
            vec![GrantEntity {
                name: "house_mode".to_string(),
                room: "global".to_string(),
                capability: "switch".to_string(),
                write: WriteMode::Shared,
                owner: "modes".to_string(),
            }]
        );
    }

    #[test]
    fn a_commanded_virtual_entity_nobody_listens_for_is_a_plan_error() {
        let house = house_with_latch(false);
        let (expanded, _warnings, expand_errors) = expand(&house);
        assert!(expand_errors.is_empty(), "{expand_errors:?}");

        let (_grants, _warnings, errors) = resolve(&house, &expanded);
        let codes: Vec<&str> = errors.iter().map(|e| e.code).collect();
        assert_eq!(codes, vec!["virtual-entity-commanded"], "{errors:?}");
    }

    /// The key expression is part of a grant's identity: widening `/lock`
    /// to `/**` matches the same entity yet is a different authority, so it
    /// must diff as a grant delta and render in the plan.
    #[test]
    fn widening_a_key_expression_changes_the_grant_table() {
        let house = house_with_lock(None);
        let (expanded, _, _) = expand(&house);
        let (grants, _, _) = resolve(&house, &expanded);
        let night_mode = grants.iter().find(|g| g.unit == "night_mode").unwrap();
        assert_eq!(
            night_mode.keys,
            vec!["home/cmd/hallway/lock/lock".to_string()]
        );

        let mut widened = house_with_lock(None);
        let bus = widened.units[1].manifest.bus.as_mut().unwrap();
        bus.publishes.get_mut("lock").unwrap().key = "home/cmd/hallway/lock/**".to_string();
        let (expanded, _, _) = expand(&widened);
        let (widened_grants, _, _) = resolve(&widened, &expanded);
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

    /// Every bound entity sits in its owner's state row, so a change to an
    /// entity nobody is granted onto — the lamp here — is still a grant
    /// delta (docs/design.md: entity moves and write-policy changes are
    /// structural).
    #[test]
    fn a_change_to_an_ungranted_entity_changes_the_grant_table() {
        let with_state = || {
            let mut house = house_with_lock(None);
            let bus = house.units[0].manifest.bus.as_mut().unwrap();
            bus.publishes.insert(
                "state".to_string(),
                PublishSpec {
                    key: "home/state/{room}/{entity}/**".to_string(),
                    capability: None,
                    priority: None,
                },
            );
            house
        };
        let baseline = with_state();
        let (expanded, _, _) = expand(&baseline);
        let (grants, _, _) = resolve(&baseline, &expanded);
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
        let (expanded, _, _) = expand(&moved);
        let (moved_grants, _, _) = resolve(&moved, &expanded);
        assert_ne!(
            grants, moved_grants,
            "moving the ungranted lamp must change the table"
        );

        let mut retyped = with_state();
        retyped.entities[0].file.entity.capability = "switch".to_string();
        let (expanded, _, _) = expand(&retyped);
        let (retyped_grants, _, _) = resolve(&retyped, &expanded);
        assert_ne!(
            grants, retyped_grants,
            "a capability change must change the table"
        );
    }

    /// Exclusivity counts writers per unit: two bindings of one unit onto
    /// one exclusive entity are one writer.
    #[test]
    fn two_bindings_in_one_unit_are_one_writer() {
        let mut house = house_with_lock(None);
        house.entities[0].file.write_policy.mode = WriteMode::Exclusive;
        let bus = house.units[1].manifest.bus.as_mut().unwrap();
        for (name, aspect) in [("lamp_on", "on"), ("lamp_brightness", "brightness")] {
            bus.publishes.insert(
                name.to_string(),
                PublishSpec {
                    key: format!("home/cmd/kitchen/lamp/{aspect}"),
                    capability: Some("light".to_string()),
                    priority: Some(Priority::Automation),
                },
            );
        }
        let (expanded, _, _) = expand(&house);
        let (_, _, errors) = resolve(&house, &expanded);
        assert!(
            !errors.iter().any(|e| e.code == "exclusive-write-conflict"),
            "{errors:?}"
        );
    }

    /// The manual band is the family's; an automation claiming it plans,
    /// but the plan says so.
    #[test]
    fn manual_band_on_an_automation_is_a_plan_warning() {
        let mut house = house_with_lock(None);
        let bus = house.units[1].manifest.bus.as_mut().unwrap();
        bus.publishes.get_mut("lock").unwrap().priority = Some(Priority::Manual);
        let (expanded, _, _) = expand(&house);
        let (_, warnings, _) = resolve(&house, &expanded);
        assert!(
            warnings.iter().any(|w| w.starts_with(
                "publish night_mode.lock declares priority \"manual\" on automation \"night_mode\""
            )),
            "{warnings:?}"
        );
    }

    #[test]
    fn arbitrated_entity_with_no_arbiter_publish_is_a_plan_error() {
        let house = house_with_lock(None);
        let (expanded, _warnings, expand_errors) = expand(&house);
        assert!(expand_errors.is_empty(), "{expand_errors:?}");

        let (_grants, _warnings, errors) = resolve(&house, &expanded);
        assert!(
            errors
                .iter()
                .any(|e| e.code == "arbitrated-uncovered" && e.subject == "lock"),
            "{errors:?}"
        );
    }
}
