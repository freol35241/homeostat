use crate::error::ValidationError;
use crate::keyspace::{KeyExpr, Segment};
use crate::manifest::{UnitKind, WriteMode};
use crate::repo::House;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Publishes,
    Subscribes,
}

impl Direction {
    pub fn verb(&self) -> &'static str {
        match self {
            Direction::Publishes => "publishes",
            Direction::Subscribes => "subscribes",
        }
    }
}

/// One `[bus]` entry with its plan-time expansion: templates substituted per
/// bound entity, zone references expanded to one expression per room.
#[derive(Debug)]
pub struct ExpandedKey {
    pub unit: String,
    pub kind: UnitKind,
    pub entry: String,
    pub direction: Direction,
    /// The expression as written in the manifest.
    pub source: String,
    /// Whether the source used {room}/{entity} templates — carried here so
    /// consumers never re-parse `source` to recover it.
    pub templated: bool,
    /// Zone name, when the room slot referenced a zone.
    pub zone: Option<String>,
    pub exprs: Vec<KeyExpr>,
}

/// Expands every bus entry of every unit. Keys that fail to parse or fall
/// outside the key-space schema produce errors and no expansion; a zone
/// reference that expands to no rooms produces a warning — the entry is
/// silently dead otherwise.
pub fn expand(house: &House) -> (Vec<ExpandedKey>, Vec<String>, Vec<ValidationError>) {
    let mut expanded = Vec::new();
    let mut warnings = Vec::new();
    let mut errors = Vec::new();

    for unit in &house.units {
        let Some(bus) = &unit.manifest.bus else {
            continue;
        };
        let entries = bus
            .publishes
            .iter()
            .map(|(name, spec)| (name, spec.key.as_str(), Direction::Publishes))
            .chain(
                bus.subscribes
                    .iter()
                    .map(|(name, key)| (name, key.as_str(), Direction::Subscribes)),
            );
        for (entry, raw, direction) in entries {
            let subject = format!("{}.{entry}", unit.manifest.unit.name);
            let expr = match KeyExpr::parse(raw).and_then(|e| e.check_schema(raw).map(|()| e)) {
                Ok(expr) => expr,
                Err(message) => {
                    errors.push(ValidationError::new(
                        "key-outside-schema",
                        subject,
                        message,
                        Some(unit.path.clone()),
                    ));
                    continue;
                }
            };

            let templated = expr.has_template();
            let mut zone = None;
            let exprs = if templated {
                if !matches!(
                    unit.manifest.unit.kind,
                    UnitKind::Adapter | UnitKind::Automation
                ) {
                    errors.push(ValidationError::new(
                        "template-outside-binding-unit",
                        subject,
                        format!("\"{raw}\" uses {{room}}/{{entity}} templates, which only entity-binding units (adapters and automations) may use"),
                        Some(unit.path.clone()),
                    ));
                    continue;
                }
                // An adapter physically lacks a cmd path to an arbitrated
                // entity: its templated cmd expands only over the
                // non-arbitrated entities it binds, and a templated
                // arbiter-class expression (receiving the arbiter's
                // forwarded, post-arbitration commands) only over the
                // arbitrated ones.
                house
                    .entities
                    .iter()
                    .filter(|e| e.owner == unit.manifest.unit.name)
                    .filter(|e| match expr.class() {
                        Some("cmd") => e.file.write_policy.mode != WriteMode::Arbitrated,
                        Some("arbiter") => e.file.write_policy.mode == WriteMode::Arbitrated,
                        _ => true,
                    })
                    .map(|e| expr.substitute(&e.file.entity.room, &e.name))
                    .collect()
            } else if let Some(Segment::Literal(room)) = expr.room_slot() {
                if let Some(rooms) = house.zones.get(room) {
                    zone = Some(room.clone());
                    rooms.iter().map(|r| expr.with_room(r)).collect()
                } else {
                    vec![expr]
                }
            } else {
                vec![expr]
            };

            if exprs.is_empty() {
                if let Some(zone) = &zone {
                    warnings.push(format!(
                        "{} {subject} matches nothing: zone \"{zone}\" has no rooms",
                        direction.verb()
                    ));
                } else if templated
                    && !house
                        .entities
                        .iter()
                        .any(|e| e.owner == unit.manifest.unit.name)
                {
                    // A template over nothing is the same silent death as the
                    // zone case: the unit subscribes to nothing, may publish
                    // nothing, and reports healthy. Only when the unit binds
                    // no entities at all, though — a templated cmd that
                    // expands to nothing because every bound entity is
                    // arbitrated is correct, and the arbiter carries those.
                    // A missing `[entities]` table can only ever be a
                    // mistake, so it is an error; a table that is present but
                    // yields nothing is a house being built up, so it warns.
                    if unit.manifest.entities.is_none() {
                        errors.push(ValidationError::new(
                            "template-without-entities",
                            subject,
                            format!(
                                "\"{raw}\" uses {{room}}/{{entity}} templates but the unit has no [entities] table, so it expands to nothing"
                            ),
                            Some(unit.path.clone()),
                        ));
                    } else {
                        warnings.push(format!(
                            "{} {subject} matches nothing: unit binds no entities",
                            direction.verb()
                        ));
                    }
                }
            }
            expanded.push(ExpandedKey {
                unit: unit.manifest.unit.name.clone(),
                kind: unit.manifest.unit.kind,
                entry: entry.clone(),
                direction,
                source: raw.to_string(),
                templated,
                zone,
                exprs,
            });
        }
    }

    (expanded, warnings, errors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{
        BusSection, EntitiesSection, EntityFile, EntitySection, RestartPolicy, RuntimeSection,
        UnitManifest, UnitSection, WritePolicy,
    };
    use crate::repo::{LoadedEntity, LoadedUnit};
    use std::collections::BTreeMap;

    fn adapter_unit(name: &str, bus: BusSection) -> LoadedUnit {
        LoadedUnit {
            manifest: UnitManifest {
                schema: 1,
                unit: UnitSection {
                    name: name.to_string(),
                    kind: UnitKind::Adapter,
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

    #[test]
    fn adapter_template_splits_cmd_and_arbiter_by_write_mode() {
        let mut subscribes = BTreeMap::new();
        subscribes.insert(
            "commands".to_string(),
            "home/cmd/{room}/{entity}/**".to_string(),
        );
        subscribes.insert(
            "arbiter_commands".to_string(),
            "home/arbiter/{room}/{entity}/**".to_string(),
        );
        let bus = BusSection {
            subscribes,
            publishes: BTreeMap::new(),
        };

        let house = House {
            units: vec![adapter_unit("zigbee", bus)],
            entities: vec![
                entity("lamp", "kitchen", "light", WriteMode::Shared, "zigbee"),
                entity("lock", "hallway", "lock", WriteMode::Arbitrated, "zigbee"),
            ],
            zones: BTreeMap::new(),
        };

        let (expanded, _warnings, errors) = expand(&house);
        assert!(errors.is_empty(), "{errors:?}");

        let cmd = expanded.iter().find(|k| k.entry == "commands").unwrap();
        assert_eq!(
            cmd.exprs,
            vec![KeyExpr::parse("home/cmd/kitchen/lamp/**").unwrap()]
        );

        let arbiter = expanded
            .iter()
            .find(|k| k.entry == "arbiter_commands")
            .unwrap();
        assert_eq!(
            arbiter.exprs,
            vec![KeyExpr::parse("home/arbiter/hallway/lock/**").unwrap()]
        );
    }

    #[test]
    fn cmd_template_expands_to_nothing_without_error_when_all_bound_entities_are_arbitrated() {
        let mut subscribes = BTreeMap::new();
        subscribes.insert(
            "commands".to_string(),
            "home/cmd/{room}/{entity}/**".to_string(),
        );
        let bus = BusSection {
            subscribes,
            publishes: BTreeMap::new(),
        };

        let house = House {
            units: vec![adapter_unit("zigbee", bus)],
            entities: vec![entity(
                "lock",
                "hallway",
                "lock",
                WriteMode::Arbitrated,
                "zigbee",
            )],
            zones: BTreeMap::new(),
        };

        let (expanded, _warnings, errors) = expand(&house);
        assert!(errors.is_empty(), "{errors:?}");
        let cmd = expanded.iter().find(|k| k.entry == "commands").unwrap();
        assert!(cmd.exprs.is_empty());
    }

    /// A templated key in a unit that binds nothing is #68's silence in the
    /// core: it plans clean, applies clean, and the unit is deaf. The
    /// `[entities]` table is the tell — forgotten outright is a mistake,
    /// present but empty is a house part-way built.
    #[test]
    fn a_template_in_a_unit_with_no_entities_table_is_an_error() {
        let mut subscribes = BTreeMap::new();
        subscribes.insert(
            "commands".to_string(),
            "home/cmd/{room}/{entity}/**".to_string(),
        );
        let bus = BusSection {
            subscribes,
            publishes: BTreeMap::new(),
        };

        let house = House {
            units: vec![adapter_unit("latches", bus)],
            entities: vec![],
            zones: BTreeMap::new(),
        };

        let (_expanded, warnings, errors) = expand(&house);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].code, "template-without-entities");
        assert_eq!(errors[0].subject, "latches.commands");
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn a_template_in_a_unit_whose_entities_dir_is_empty_warns() {
        let mut subscribes = BTreeMap::new();
        subscribes.insert(
            "commands".to_string(),
            "home/cmd/{room}/{entity}/**".to_string(),
        );
        let bus = BusSection {
            subscribes,
            publishes: BTreeMap::new(),
        };

        let mut unit = adapter_unit("latches", bus);
        unit.manifest.entities = Some(EntitiesSection {
            dir: "entities/latches/".to_string(),
        });

        let house = House {
            units: vec![unit],
            entities: vec![],
            zones: BTreeMap::new(),
        };

        let (_expanded, warnings, errors) = expand(&house);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(
            warnings,
            vec!["subscribes latches.commands matches nothing: unit binds no entities"]
        );
    }
}
