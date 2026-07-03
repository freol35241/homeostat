use std::collections::BTreeMap;

use crate::error::ValidationError;
use crate::keyspace::{is_reserved_word, PSEUDO_ROOMS};
use crate::manifest::{
    DiscoveryMode, ParamSpec, ParamType, UnitKind, CAPABILITIES,
};
use crate::repo::House;

/// Structural validation that does not involve key expansion or grants:
/// uniqueness, capabilities, reserved rooms, ownership, zones, params.
pub fn validate(house: &House) -> Vec<ValidationError> {
    let mut errors = Vec::new();

    check_duplicates(house, &mut errors);
    check_manifest_shape(house, &mut errors);
    check_entities(house, &mut errors);
    check_zones(house, &mut errors);
    check_params(house, &mut errors);

    errors
}

fn check_duplicates(house: &House, errors: &mut Vec<ValidationError>) {
    let mut unit_paths: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for unit in &house.units {
        unit_paths
            .entry(&unit.manifest.unit.name)
            .or_default()
            .push(&unit.path);
    }
    for (name, mut paths) in unit_paths {
        if paths.len() > 1 {
            paths.sort();
            errors.push(ValidationError::new(
                "duplicate-unit-name",
                name,
                format!("defined in {}", paths.join(" and ")),
                None,
            ));
        }
    }

    let mut entity_paths: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for entity in &house.entities {
        entity_paths.entry(&entity.name).or_default().push(&entity.path);
    }
    for (name, mut paths) in entity_paths {
        if paths.len() > 1 {
            paths.sort();
            errors.push(ValidationError::new(
                "duplicate-entity-name",
                name,
                format!("defined in {}", paths.join(" and ")),
                None,
            ));
        }
    }
}

fn check_manifest_shape(house: &House, errors: &mut Vec<ValidationError>) {
    for unit in &house.units {
        let name = &unit.manifest.unit.name;
        let file = Some(unit.path.clone());
        if unit.manifest.unit.kind == UnitKind::Adapter {
            if unit.manifest.entities.is_none() {
                errors.push(ValidationError::new(
                    "invalid-manifest",
                    name,
                    "adapter requires an [entities] section",
                    file.clone(),
                ));
            }
            match &unit.manifest.discovery {
                None => errors.push(ValidationError::new(
                    "invalid-manifest",
                    name,
                    "adapter requires a [discovery] section",
                    file.clone(),
                )),
                Some(d) => {
                    let missing = match d.mode {
                        DiscoveryMode::Static if d.endpoint.is_none() => {
                            Some("discovery mode \"static\" requires an endpoint")
                        }
                        DiscoveryMode::Mdns if d.service.is_none() => {
                            Some("discovery mode \"mdns\" requires a service")
                        }
                        _ => None,
                    };
                    if let Some(message) = missing {
                        errors.push(ValidationError::new(
                            "invalid-manifest",
                            name,
                            message,
                            file.clone(),
                        ));
                    }
                }
            }
        } else {
            if unit.manifest.entities.is_some() {
                errors.push(ValidationError::new(
                    "invalid-manifest",
                    name,
                    "[entities] is only valid for adapters",
                    file.clone(),
                ));
            }
            if unit.manifest.discovery.is_some() {
                errors.push(ValidationError::new(
                    "invalid-manifest",
                    name,
                    "[discovery] is only valid for adapters",
                    file.clone(),
                ));
            }
        }
    }
}

fn check_entities(house: &House, errors: &mut Vec<ValidationError>) {
    for entity in &house.entities {
        let file = Some(entity.path.clone());
        let capability = &entity.file.entity.capability;
        if !CAPABILITIES.contains(&capability.as_str()) {
            errors.push(ValidationError::new(
                "unknown-capability",
                &entity.name,
                format!("unknown capability \"{capability}\""),
                file.clone(),
            ));
        }

        let room = &entity.file.entity.room;
        if is_reserved_word(room) && !PSEUDO_ROOMS.contains(&room.as_str()) {
            errors.push(ValidationError::new(
                "reserved-room-name",
                &entity.name,
                format!("room \"{room}\" is a reserved word"),
                file.clone(),
            ));
        }

        let owner = &entity.file.write_policy.owner;
        match house.unit(owner) {
            None => errors.push(ValidationError::new(
                "missing-owner-adapter",
                &entity.name,
                format!("owner adapter \"{owner}\" does not exist"),
                file.clone(),
            )),
            Some(unit) if unit.manifest.unit.kind != UnitKind::Adapter => {
                errors.push(ValidationError::new(
                    "missing-owner-adapter",
                    &entity.name,
                    format!("owner \"{owner}\" is not an adapter"),
                    file.clone(),
                ));
            }
            Some(_) if owner != &entity.adapter => {
                errors.push(ValidationError::new(
                    "owner-mismatch",
                    &entity.name,
                    format!(
                        "owner \"{owner}\" but bound by adapter \"{}\"",
                        entity.adapter
                    ),
                    file.clone(),
                ));
            }
            Some(_) => {}
        }
    }
}

fn check_zones(house: &House, errors: &mut Vec<ValidationError>) {
    let rooms = house.rooms();
    let file = Some("zones.toml".to_string());
    for (zone, members) in &house.zones {
        if is_reserved_word(zone) {
            errors.push(ValidationError::new(
                "reserved-zone-name",
                zone,
                format!("zone name \"{zone}\" is a reserved word"),
                file.clone(),
            ));
            continue;
        }
        if rooms.contains(&zone.as_str()) {
            errors.push(ValidationError::new(
                "zone-room-collision",
                zone,
                format!("zone name \"{zone}\" collides with a room name"),
                file.clone(),
            ));
        }
        for room in members {
            if !rooms.contains(&room.as_str()) {
                errors.push(ValidationError::new(
                    "zone-unknown-room",
                    zone,
                    format!("zone references unknown room \"{room}\""),
                    file.clone(),
                ));
            }
        }
    }
}

fn check_params(house: &House, errors: &mut Vec<ValidationError>) {
    for unit in &house.units {
        let Some(params) = &unit.manifest.params else { continue };
        for (name, spec) in params {
            let subject = format!("{}.{name}", unit.manifest.unit.name);
            check_param(&subject, spec, &unit.path, errors);
        }
    }
}

fn check_param(subject: &str, spec: &ParamSpec, path: &str, errors: &mut Vec<ValidationError>) {
    let mut err = |code: &'static str, message: String| {
        errors.push(ValidationError::new(
            code,
            subject,
            message,
            Some(path.to_string()),
        ));
    };
    let t = spec.param_type;

    let default_ok = match (t, &spec.default) {
        (ParamType::Bool, toml::Value::Boolean(_)) => true,
        (ParamType::Int, toml::Value::Integer(_)) => true,
        (ParamType::Float, toml::Value::Float(_) | toml::Value::Integer(_)) => true,
        (ParamType::String, toml::Value::String(_)) => true,
        (ParamType::Time, toml::Value::String(s)) => parse_time(s).is_some(),
        _ => false,
    };
    if !default_ok {
        err(
            "invalid-default",
            format!("default {} does not match type \"{t}\"", display_value(&spec.default)),
        );
    }

    let Some(constraint) = &spec.constraint else { return };
    for (key, value) in constraint {
        let valid_for_type = match key.as_str() {
            "min" | "max" => matches!(t, ParamType::Int | ParamType::Float),
            "after" | "before" => t == ParamType::Time,
            "enum" => t == ParamType::String,
            _ => {
                err(
                    "malformed-constraint",
                    format!("unknown constraint \"{key}\""),
                );
                continue;
            }
        };
        if !valid_for_type {
            err(
                "malformed-constraint",
                format!("constraint \"{key}\" is not valid for type \"{t}\""),
            );
            continue;
        }
        let value_ok = match key.as_str() {
            "min" | "max" => matches!(value, toml::Value::Integer(_) | toml::Value::Float(_)),
            "after" | "before" => {
                matches!(value, toml::Value::String(s) if parse_time(s).is_some())
            }
            "enum" => matches!(
                value,
                toml::Value::Array(items)
                    if !items.is_empty() && items.iter().all(|i| i.is_str())
            ),
            _ => unreachable!(),
        };
        if !value_ok {
            let expected = match key.as_str() {
                "min" | "max" => "a number",
                "after" | "before" => "a time (HH:MM)",
                _ => "a non-empty array of strings",
            };
            err(
                "malformed-constraint",
                format!("constraint \"{key}\" must be {expected}"),
            );
        }
    }

    if let (Some(min), Some(max)) = (
        constraint.get("min").and_then(as_number),
        constraint.get("max").and_then(as_number),
    ) {
        if min > max {
            err(
                "malformed-constraint",
                format!(
                    "min ({}) is greater than max ({})",
                    display_value(&constraint["min"]),
                    display_value(&constraint["max"])
                ),
            );
        }
    }
}

fn as_number(value: &toml::Value) -> Option<f64> {
    match value {
        toml::Value::Integer(i) => Some(*i as f64),
        toml::Value::Float(f) => Some(*f),
        _ => None,
    }
}

/// Renders a TOML value without string quotes, for error messages and plan
/// output.
pub fn display_value(value: &toml::Value) -> String {
    match value {
        toml::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn parse_time(s: &str) -> Option<(u8, u8)> {
    let (hh, mm) = s.split_once(':')?;
    let hh: u8 = hh.parse().ok()?;
    let mm: u8 = mm.parse().ok()?;
    (hh < 24 && mm < 60).then_some((hh, mm))
}
