//! Structural validation of a loaded house: names, uniqueness, manifest
//! shape, zones and parameters. Per-entity semantics live in `entity` and
//! the `dashboard.toml` checks in `dashboard`. Checks over expanded keys or
//! the grant table live in `expand`, `grants`, `publish_rules` and `wiring`. The
//! exception is whether some unit publishes the house clock, which the
//! time-of-day checks in `entity` need.

mod dashboard;
mod entity;

use std::collections::BTreeMap;

use crate::error::{Code, Diagnostics};
use crate::expand::ExpandedKey;
use crate::keyspace::{is_reserved_word, PSEUDO_ROOMS};
use crate::manifest::{DiscoveryMode, ParamType, UnitKind, ENABLED_PARAM};
use crate::params::Constraint;
use crate::repo::House;

use dashboard::check_dashboard;
use entity::{check_aspect_intervals, check_entities, check_persons, has_clock};

/// Structural validation that does not involve grants: uniqueness,
/// capabilities, reserved rooms, ownership, zones, params. `expanded` is the
/// house's expanded keys, from which it learns whether the house has a clock.
pub fn validate(house: &House, expanded: &[ExpandedKey], diags: &mut Diagnostics) {
    check_names(house, diags);
    check_duplicates(house, diags);
    check_manifest_shape(house, diags);
    check_entities(house, has_clock(expanded), diags);
    check_persons(house, diags);
    check_aspect_intervals(house, diags);
    check_zones(house, diags);
    check_params(house, diags);
    check_dashboard(house, diags);
}

/// Whether a name is usable as one bus key segment. Other names break the
/// fixed key schema (`/`), mean something to the bus (`*`, `$`, `?`, `#`), or
/// risk whitespace and encoding problems. A bad unit name would also panic the
/// supervisor's liveliness subscriber. `.` and `..` are refused as path-like.
/// The SDK refuses the same names (`tests/fixtures/key_segments.json`), so a
/// name that passes here is one every unit can build keys from.
pub(crate) fn valid_segment(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

/// Every name that becomes a key segment: unit and parameter names
/// (`home/config/{unit}/{param}`, `home/meta/{unit}/...`), entity and room
/// names, the aspect names an entity file declares
/// (`home/state/{room}/{entity}/{aspect}`), and zone names (expanded in
/// key expressions). The unit name `system` is refused because the core serves
/// `home/meta/system/**` itself.
fn check_names(house: &House, diags: &mut Diagnostics) {
    let segment_message = |what: &str, name: &str| {
        format!(
            "{what} \"{name}\" must be a single key segment (letters, digits, \"_\", \"-\", \".\"; not \".\" or \"..\")"
        )
    };
    for unit in &house.units {
        let name = &unit.manifest.unit.name;
        let mut diags = diags.in_file(&unit.path);
        if !valid_segment(name) {
            diags.error(Code::InvalidName, name, segment_message("unit name", name));
        } else if name == "system" {
            diags.error(
                Code::ReservedUnitName,
                name,
                "unit name \"system\" is reserved for the core's meta keys",
            );
        }
        if let Some(params) = &unit.manifest.params {
            for param in params.keys() {
                if !valid_segment(param) {
                    diags.error(
                        Code::InvalidName,
                        format!("{name}.{param}"),
                        segment_message("parameter name", param),
                    );
                }
            }
        }
    }
    for entity in &house.entities {
        let mut diags = diags.in_file(&entity.path);
        if !valid_segment(&entity.name) {
            diags.error(
                Code::InvalidName,
                &entity.name,
                segment_message("entity name", &entity.name),
            );
        }
        for aspect in entity.file.aspects.iter().flat_map(|a| a.keys()) {
            if !valid_segment(aspect) {
                diags.error(
                    Code::InvalidName,
                    format!("{}.{aspect}", entity.name),
                    segment_message("aspect name", aspect),
                );
            }
        }
        let room = &entity.file.entity.room;
        if !valid_segment(room) {
            diags.error(
                Code::InvalidName,
                &entity.name,
                segment_message("room", room),
            );
        }
    }
    for zone in house.zones.keys() {
        if !valid_segment(zone) {
            diags.in_file("zones.toml").error(
                Code::InvalidName,
                zone,
                segment_message("zone name", zone),
            );
        }
    }
    for view in house.dashboard.iter().flat_map(|d| &d.view) {
        if !valid_segment(&view.name) {
            diags.in_file("dashboard.toml").error(
                Code::InvalidName,
                &view.name,
                segment_message("view name", &view.name),
            );
        }
    }
}

fn check_duplicates(house: &House, diags: &mut Diagnostics) {
    let mut unit_paths: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for unit in &house.units {
        unit_paths
            .entry(&unit.manifest.unit.name)
            .or_default()
            .push(&unit.path);
    }
    for (name, mut paths) in unit_paths {
        if paths.len() > 1 {
            paths.sort_unstable();
            diags.error(
                Code::DuplicateUnitName,
                name,
                format!("defined in {}", paths.join(" and ")),
            );
        }
    }

    let mut entity_paths: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for entity in &house.entities {
        entity_paths
            .entry(&entity.name)
            .or_default()
            .push(&entity.path);
    }
    for (name, mut paths) in entity_paths {
        if paths.len() > 1 {
            paths.sort_unstable();
            diags.error(
                Code::DuplicateEntityName,
                name,
                format!("defined in {}", paths.join(" and ")),
            );
        }
    }

    // The binding `id` is the device address within one owner's namespace. Two
    // files sharing it would collapse to whichever one the adapter's `by_id`
    // map keeps last.
    let mut entity_ids: BTreeMap<(&str, &str), Vec<&str>> = BTreeMap::new();
    for entity in &house.entities {
        let Some(id) = &entity.file.entity.id else {
            continue;
        };
        entity_ids
            .entry((&entity.owner, id.as_str()))
            .or_default()
            .push(&entity.path);
    }
    for ((owner, id), mut paths) in entity_ids {
        if paths.len() > 1 {
            paths.sort_unstable();
            diags.error(
                Code::DuplicateEntityId,
                id,
                format!("bound to \"{owner}\" by {}", paths.join(" and ")),
            );
        }
    }
}

fn check_manifest_shape(house: &House, diags: &mut Diagnostics) {
    for unit in &house.units {
        let name = &unit.manifest.unit.name;
        let mut diags = diags.in_file(&unit.path);
        if let Some(d) = &unit.manifest.discovery {
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
                diags.error(Code::InvalidManifest, name, message);
            }
        }
        if unit.manifest.unit.kind == UnitKind::Automation {
            let enabled = unit
                .manifest
                .params
                .as_ref()
                .and_then(|p| p.get(ENABLED_PARAM));
            if enabled.is_some_and(|spec| spec.param_type != ParamType::Bool) {
                diags.error(
                    Code::InvalidManifest,
                    name,
                    "an automation's \"enabled\" parameter pauses it, so it must be a bool",
                );
            }
        }
        if unit.manifest.unit.kind == UnitKind::Adapter {
            if unit.manifest.entities.is_none() {
                diags.error(
                    Code::InvalidManifest,
                    name,
                    "adapter requires an [entities] section",
                );
            }
            if unit.manifest.discovery.is_none() {
                diags.error(
                    Code::InvalidManifest,
                    name,
                    "adapter requires a [discovery] section",
                );
            }
        } else {
            // Automations may bind entities (virtual sensors). Services have
            // no need to, and are refused until one does.
            if unit.manifest.entities.is_some() && unit.manifest.unit.kind != UnitKind::Automation {
                diags.error(
                    Code::InvalidManifest,
                    name,
                    "[entities] is only valid for adapters and automations",
                );
            }
            // Services may talk to an external backend (e.g. the recorder's
            // store) and use [discovery] the same way adapters do.
            if unit.manifest.discovery.is_some() && unit.manifest.unit.kind != UnitKind::Service {
                diags.error(
                    Code::InvalidManifest,
                    name,
                    "[discovery] is only valid for adapters and services",
                );
            }
        }
    }
}

fn check_zones(house: &House, diags: &mut Diagnostics) {
    let rooms = house.rooms();
    let mut diags = diags.in_file("zones.toml");
    for (zone, members) in &house.zones {
        if is_reserved_word(zone) {
            diags.error(
                Code::ReservedZoneName,
                zone,
                format!("zone name \"{zone}\" is a reserved word"),
            );
            continue;
        }
        if rooms.contains(&zone.as_str()) {
            diags.error(
                Code::ZoneRoomCollision,
                zone,
                format!("zone name \"{zone}\" collides with a room name"),
            );
        }
        for room in members {
            if PSEUDO_ROOMS.contains(&room.as_str()) {
                diags.error(
                    Code::ZonePseudoRoom,
                    zone,
                    format!("zone includes pseudo-room \"{room}\""),
                );
            } else if !rooms.contains(&room.as_str()) {
                diags.error(
                    Code::ZoneUnknownRoom,
                    zone,
                    format!("zone references unknown room \"{room}\""),
                );
            }
        }
    }
}

fn check_params(house: &House, diags: &mut Diagnostics) {
    for unit in &house.units {
        let Some(params) = &unit.manifest.params else {
            continue;
        };
        let mut diags = diags.in_file(&unit.path);
        for (name, spec) in params {
            let subject = format!("{}.{name}", unit.manifest.unit.name);
            if let Err(errors) = Constraint::parse(spec) {
                for (code, message) in errors {
                    diags.error(code, &subject, message);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::valid_segment;

    /// The core reads a key segment as the SDK does
    /// (`tests/fixtures/key_segments.json`).
    #[test]
    fn the_key_segment_rule_is_the_shared_one() {
        let cases: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/key_segments.json")).unwrap();
        for s in cases["accepted"].as_array().unwrap() {
            let s = s.as_str().unwrap();
            assert!(valid_segment(s), "{s:?} is a key segment");
        }
        for s in cases["refused"].as_array().unwrap() {
            let s = s.as_str().unwrap();
            assert!(!valid_segment(s), "{s:?} is not a key segment");
        }
    }
}
