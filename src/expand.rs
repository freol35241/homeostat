//! Plan-time key expansion: every `[bus]` expression of every unit, with
//! `{room}`/`{entity}` templates substituted per bound entity and a zone in
//! the room slot expanded to one expression per member room. The SDK
//! performs the same expansion at runtime, so what a unit subscribes to is
//! what `plan` printed for it.

use crate::error::{Code, ValidationError};
use crate::keyspace::{KeyExpr, Segment};
use crate::manifest::{UnitKind, WriteMode};
use crate::repo::House;

/// Whether a `[bus]` entry is a publish or a subscribe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Publishes,
    Subscribes,
}

impl Direction {
    /// The verb plan output uses for this direction.
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
    /// Whether the source used {room}/{entity} templates. Stored here so
    /// consumers do not re-parse `source` to recover it.
    pub templated: bool,
    /// Zone name, when the room slot referenced a zone.
    pub zone: Option<String>,
    pub exprs: Vec<KeyExpr>,
}

/// Expands every bus entry of every unit. Keys that fail to parse or fall
/// outside the key-space schema produce errors and no expansion. A zone
/// reference that expands to no rooms produces a warning, because otherwise
/// the entry would be dead without anyone noticing.
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
                        Code::KeyOutsideSchema,
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
                        Code::TemplateOutsideBindingUnit,
                        subject,
                        format!("\"{raw}\" uses {{room}}/{{entity}} templates, which only entity-binding units (adapters and automations) may use"),
                        Some(unit.path.clone()),
                    ));
                    continue;
                }
                // An adapter has no cmd path to an arbitrated entity. Its
                // templated cmd expands only over the non-arbitrated entities
                // it binds. A templated arbiter-class expression, which
                // receives the arbiter's forwarded post-arbitration commands,
                // expands only over the arbitrated ones.
                house
                    .entities
                    .iter()
                    .filter(|e| e.owner == unit.manifest.unit.name)
                    .filter(|e| match expr.class() {
                        Some("cmd") => e.file.write_policy.mode() != WriteMode::Arbitrated,
                        Some("arbiter") => e.file.write_policy.mode() == WriteMode::Arbitrated,
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
                    // A template over nothing fails quietly, as in the zone
                    // case: the unit subscribes to nothing, may publish
                    // nothing, and reports healthy. This applies only when the
                    // unit binds no entities at all. A templated cmd that
                    // expands to nothing because every bound entity is
                    // arbitrated is correct, since the arbiter carries those.
                    // A missing `[entities]` table is always a mistake, so it
                    // is an error. A table that is present but yields nothing
                    // is a house being built up, so it warns.
                    if unit.manifest.entities.is_none() {
                        errors.push(ValidationError::new(
                            Code::TemplateWithoutEntities,
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

    const LAMP: &str = r#"
        schema = 1

        [entity]
        id = "lamp"
        capability = "light"
        room = "kitchen"

        [write_policy]
        mode = "shared"
        owner = "zigbee"
    "#;

    const LOCK: &str = r#"
        schema = 1

        [entity]
        id = "lock"
        capability = "lock"
        room = "hallway"

        [write_policy]
        mode = "arbitrated"
        owner = "zigbee"
    "#;

    #[test]
    fn adapter_template_splits_cmd_and_arbiter_by_write_mode() {
        let zigbee = r#"
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
        let house = House::from_toml(
            &[("zigbee", zigbee)],
            &[("zigbee", "lamp", LAMP), ("zigbee", "lock", LOCK)],
        );

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
        let zigbee = r#"
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
        "#;
        let house = House::from_toml(&[("zigbee", zigbee)], &[("zigbee", "lock", LOCK)]);

        let (expanded, _warnings, errors) = expand(&house);
        assert!(errors.is_empty(), "{errors:?}");
        let cmd = expanded.iter().find(|k| k.entry == "commands").unwrap();
        assert!(cmd.exprs.is_empty());
    }

    /// A templated key in a unit that binds nothing fails quietly: it plans
    /// clean, applies clean, and the unit hears nothing. The `[entities]`
    /// table tells the cases apart. A missing table is a mistake. A present
    /// but empty one is a house part-way built.
    #[test]
    fn a_template_in_a_unit_with_no_entities_table_is_an_error() {
        let latches = r#"
            schema = 1

            [unit]
            name = "latches"
            kind = "adapter"

            [runtime]
            command = "true"
            restart = "always"

            [bus.subscribes]
            commands = "home/cmd/{room}/{entity}/**"
        "#;
        let house = House::from_toml(&[("latches", latches)], &[]);

        let (_expanded, warnings, errors) = expand(&house);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].code, Code::TemplateWithoutEntities);
        assert_eq!(errors[0].subject, "latches.commands");
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn a_template_in_a_unit_whose_entities_dir_is_empty_warns() {
        let latches = r#"
            schema = 1

            [unit]
            name = "latches"
            kind = "adapter"

            [runtime]
            command = "true"
            restart = "always"

            [entities]
            dir = "entities/latches/"

            [bus.subscribes]
            commands = "home/cmd/{room}/{entity}/**"
        "#;
        let house = House::from_toml(&[("latches", latches)], &[]);

        let (_expanded, warnings, errors) = expand(&house);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(
            warnings,
            vec!["subscribes latches.commands matches nothing: unit binds no entities"]
        );
    }
}
