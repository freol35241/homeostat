use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::ValidationError;
use crate::expand::{Direction, ExpandedKey};
use crate::manifest::{Priority, UnitKind, WriteMode, CAPABILITIES};
use crate::repo::House;

/// One resolved write grant: a non-adapter publish expression resolved
/// against the concrete entity set. The table doubles as the dependency
/// graph (unit -> entities -> owner adapters).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    pub unit: String,
    pub publish: String,
    pub capability: String,
    pub priority: Priority,
    /// Names of granted entities (key match + capability match), sorted.
    pub entities: Vec<String>,
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
        if key.kind == UnitKind::Adapter || key.direction != Direction::Publishes {
            continue;
        }
        if !key.exprs.iter().any(|e| e.class() == Some("cmd")) {
            continue;
        }
        let unit = house.unit(&key.unit).expect("expanded key from loaded unit");
        let spec = &unit.manifest.bus.as_ref().expect("unit has bus").publishes[&key.entry];
        let subject = format!("{}.{}", key.unit, key.entry);

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

        let mut granted: Vec<String> = house
            .entities
            .iter()
            .filter(|e| e.file.entity.capability == capability)
            .filter(|e| {
                let prefix = ["home", "cmd", e.file.entity.room.as_str(), e.name.as_str()];
                key.exprs.iter().any(|expr| expr.matches_prefix(&prefix))
            })
            .map(|e| e.name.clone())
            .collect();
        granted.sort();
        granted.dedup();

        if granted.is_empty() {
            warnings.push(format!("publish {subject} matches no entities"));
        }
        grants.push(Grant {
            unit: key.unit.clone(),
            publish: key.entry.clone(),
            capability,
            priority: spec.priority.unwrap_or(Priority::Automation),
            entities: granted,
        });
    }

    grants.sort_by(|a, b| (&a.unit, &a.publish).cmp(&(&b.unit, &b.publish)));

    // Write-policy enforcement: two writers on an exclusive entity is an error.
    // Exclusivity constrains the automation band only; manual-band units
    // (dashboard, voice) sit above it by construction and never count.
    let mut writers: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for grant in &grants {
        if grant.priority == Priority::Manual {
            continue;
        }
        for entity in &grant.entities {
            writers
                .entry(entity)
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
                errors.push(ValidationError::new(
                    "exclusive-write-conflict",
                    &entity.name,
                    format!(
                        "exclusive entity has {} writers: {}",
                        writers.len(),
                        writers.join(", ")
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
        let prefix =
            ["home", "arbiter", entity.file.entity.room.as_str(), entity.name.as_str()];
        let covered = expanded.iter().any(|k| {
            k.direction == Direction::Publishes
                && k.exprs.iter().any(|e| e.class() == Some("arbiter") && e.matches_prefix(&prefix))
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
                unit: UnitSection { name: name.to_string(), kind, description: None },
                runtime: RuntimeSection {
                    command: "true".to_string(),
                    restart: RestartPolicy::Always,
                    shutdown_grace_s: None,
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
                write_policy: WritePolicy { mode, owner: adapter.to_string() },
            },
            path: format!("entities/{adapter}/{name}.toml"),
            adapter: adapter.to_string(),
        }
    }

    /// Builds a house with an arbitrated lock and a shared lamp, both bound
    /// to adapter "zigbee", plus an automation wishing to command the lock.
    /// `arbiter_bus` lets each test decide whether an arbiter-class publish
    /// covers the lock.
    fn house_with_lock(arbiter_bus: Option<BusSection>) -> House {
        let mut zigbee_subscribes = BTreeMap::new();
        zigbee_subscribes.insert("commands".to_string(), "home/cmd/{room}/{entity}/**".to_string());
        zigbee_subscribes
            .insert("arbiter_commands".to_string(), "home/arbiter/{room}/{entity}/**".to_string());
        let zigbee_bus = BusSection { subscribes: zigbee_subscribes, publishes: BTreeMap::new() };

        let mut night_mode_publishes = BTreeMap::new();
        night_mode_publishes.insert(
            "lock".to_string(),
            PublishSpec {
                key: "home/cmd/hallway/lock/lock".to_string(),
                capability: Some("lock".to_string()),
                priority: Some(Priority::Automation),
            },
        );
        let night_mode_bus = BusSection { subscribes: BTreeMap::new(), publishes: night_mode_publishes };

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
            PublishSpec { key: "home/arbiter/**".to_string(), capability: None, priority: None },
        );
        let arbiter_bus = BusSection { subscribes: BTreeMap::new(), publishes };
        let house = house_with_lock(Some(arbiter_bus));

        let (expanded, expand_errors) = expand(&house);
        assert!(expand_errors.is_empty(), "{expand_errors:?}");

        // Correct expansion split: the adapter's cmd template excludes the
        // arbitrated lock, its arbiter template includes only the lock.
        let cmd = expanded.iter().find(|k| k.entry == "commands").unwrap();
        assert_eq!(cmd.exprs, vec![KeyExpr::parse("home/cmd/kitchen/lamp/**").unwrap()]);
        let arbiter_cmd = expanded.iter().find(|k| k.entry == "arbiter_commands").unwrap();
        assert_eq!(arbiter_cmd.exprs, vec![KeyExpr::parse("home/arbiter/hallway/lock/**").unwrap()]);

        let (grants, warnings, errors) = resolve(&house, &expanded);
        assert!(errors.is_empty(), "{errors:?}");
        assert!(warnings.is_empty(), "{warnings:?}");
        let night_mode = grants.iter().find(|g| g.unit == "night_mode").unwrap();
        assert_eq!(night_mode.entities, vec!["lock".to_string()]);
    }

    #[test]
    fn arbitrated_entity_with_no_arbiter_publish_is_a_plan_error() {
        let house = house_with_lock(None);
        let (expanded, expand_errors) = expand(&house);
        assert!(expand_errors.is_empty(), "{expand_errors:?}");

        let (_grants, _warnings, errors) = resolve(&house, &expanded);
        assert!(
            errors.iter().any(|e| e.code == "arbitrated-uncovered" && e.subject == "lock"),
            "{errors:?}"
        );
    }
}
