//! Plan-time key expansion: every `[bus]` expression of every unit, with
//! `{room}`/`{entity}` templates substituted per bound entity and a zone in
//! the room slot expanded to one expression per member room. The SDK
//! performs the same expansion at runtime, so what a unit subscribes to is
//! what `plan` printed for it.

use crate::error::{Code, Diagnostics};
use crate::keyspace::{self, KeyExpr, Segment};
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
    /// The class segment of `source` (`state`, `cmd`, ...). It is read
    /// before templates are substituted, so it is known even when a
    /// template expands to nothing.
    pub class: &'static str,
    /// Whether the source used {room}/{entity} templates. Stored here so
    /// consumers do not re-parse `source` to recover it.
    pub templated: bool,
    /// Zone name, when the room slot referenced a zone.
    pub zone: Option<String>,
    pub exprs: Vec<KeyExpr>,
}

impl ExpandedKey {
    /// Whether this is a `direction` entry with an expression that can match
    /// keys under the literal `prefix` (`KeyExpr::matches_prefix`).
    pub fn reaches(&self, direction: Direction, prefix: &[&str]) -> bool {
        self.direction == direction && self.exprs.iter().any(|e| e.matches_prefix(prefix))
    }
}

/// Whether `unit`, or any unit when `None`, publishes a key under `prefix`.
pub fn publishes(expanded: &[ExpandedKey], unit: Option<&str>, prefix: &[&str]) -> bool {
    any_reaches(expanded, unit, Direction::Publishes, prefix)
}

/// Whether `unit`, or any unit when `None`, subscribes a key under `prefix`.
pub fn subscribes(expanded: &[ExpandedKey], unit: Option<&str>, prefix: &[&str]) -> bool {
    any_reaches(expanded, unit, Direction::Subscribes, prefix)
}

fn any_reaches(
    expanded: &[ExpandedKey],
    unit: Option<&str>,
    direction: Direction,
    prefix: &[&str],
) -> bool {
    expanded
        .iter()
        .any(|k| unit.is_none_or(|u| k.unit == u) && k.reaches(direction, prefix))
}

/// Expands every bus entry of every unit. Keys that fail to parse or fall
/// outside the key-space schema produce errors and no expansion. A zone
/// reference that expands to no rooms produces a warning, because otherwise
/// the entry would be dead without anyone noticing.
pub fn expand(house: &House, diags: &mut Diagnostics) -> Vec<ExpandedKey> {
    let mut expanded = Vec::new();

    for unit in &house.units {
        let mut diags = diags.in_file(&unit.path);
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
                    diags.error(Code::KeyOutsideSchema, subject, message);
                    continue;
                }
            };

            let class = expr
                .class()
                .and_then(keyspace::class)
                .expect("check_schema accepts only known classes")
                .name;
            let templated = expr.has_template();
            let mut zone = None;
            let exprs = if templated {
                if !unit.manifest.unit.kind.binds_entities() {
                    diags.error(
                        Code::TemplateOutsideBindingUnit,
                        subject,
                        format!("\"{raw}\" uses {{room}}/{{entity}} templates, which only entity-binding units (adapters and automations) may use"),
                    );
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
                    diags.warn(
                        subject,
                        format!(
                            "{} a key that matches nothing, because zone \"{zone}\" has no rooms",
                            direction.verb()
                        ),
                    );
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
                        diags.error(
                            Code::TemplateWithoutEntities,
                            subject,
                            format!(
                                "\"{raw}\" uses {{room}}/{{entity}} templates but the unit has no [entities] table, so it expands to nothing"
                            ),
                        );
                    } else {
                        diags.warn(
                            subject,
                            format!(
                                "{} a key that matches nothing, because the unit binds no entities",
                                direction.verb()
                            ),
                        );
                    }
                }
            }
            expanded.push(ExpandedKey {
                unit: unit.manifest.unit.name.clone(),
                kind: unit.manifest.unit.kind,
                entry: entry.clone(),
                direction,
                source: raw.to_string(),
                class,
                templated,
                zone,
                exprs,
            });
        }
    }

    expanded
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

        let mut diags = Diagnostics::default();
        let expanded = expand(&house, &mut diags);
        assert!(diags.errors.is_empty(), "{:?}", diags.errors);

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

        let mut diags = Diagnostics::default();
        let expanded = expand(&house, &mut diags);
        assert!(diags.errors.is_empty(), "{:?}", diags.errors);
        let cmd = expanded.iter().find(|k| k.entry == "commands").unwrap();
        assert!(cmd.exprs.is_empty());
        assert_eq!(cmd.class, "cmd");
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

        let mut diags = Diagnostics::default();
        expand(&house, &mut diags);
        let errors = &diags.errors;
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].code, Code::TemplateWithoutEntities);
        assert_eq!(errors[0].subject, "latches.commands");
        assert!(diags.warnings.is_empty(), "{:?}", diags.warnings);
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

        let mut diags = Diagnostics::default();
        expand(&house, &mut diags);
        assert!(diags.errors.is_empty(), "{:?}", diags.errors);
        let warnings: Vec<String> = diags.warnings.iter().map(ToString::to_string).collect();
        assert_eq!(
            warnings,
            vec!["latches.commands: subscribes a key that matches nothing, because the unit binds no entities (units/latches.toml)"]
        );
    }
}
