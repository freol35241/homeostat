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
    let mut writers: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for grant in &grants {
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

    (grants, warnings, errors)
}
