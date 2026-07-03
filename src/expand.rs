use crate::error::ValidationError;
use crate::keyspace::{KeyExpr, Segment};
use crate::manifest::UnitKind;
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
    /// Zone name, when the room slot referenced a zone.
    pub zone: Option<String>,
    pub exprs: Vec<KeyExpr>,
}

/// Expands every bus entry of every unit. Keys that fail to parse or fall
/// outside the key-space schema produce errors and no expansion.
pub fn expand(house: &House) -> (Vec<ExpandedKey>, Vec<ValidationError>) {
    let mut expanded = Vec::new();
    let mut errors = Vec::new();

    for unit in &house.units {
        let Some(bus) = &unit.manifest.bus else { continue };
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

            let mut zone = None;
            let exprs = if expr.has_template() {
                if unit.manifest.unit.kind != UnitKind::Adapter {
                    errors.push(ValidationError::new(
                        "template-outside-adapter",
                        subject,
                        format!("\"{raw}\" uses {{room}}/{{entity}} templates, which only adapters may use"),
                        Some(unit.path.clone()),
                    ));
                    continue;
                }
                house
                    .entities
                    .iter()
                    .filter(|e| e.adapter == unit.manifest.unit.name)
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

            expanded.push(ExpandedKey {
                unit: unit.manifest.unit.name.clone(),
                kind: unit.manifest.unit.kind,
                entry: entry.clone(),
                direction,
                source: raw.to_string(),
                zone,
                exprs,
            });
        }
    }

    (expanded, errors)
}
