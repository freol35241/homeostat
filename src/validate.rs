//! Structural validation of a loaded house: names, uniqueness, manifest
//! shape, entity ownership, zones, parameters and `dashboard.toml`.
//! Checks that need expanded keys or the grant table live in `expand` and
//! `grants`.

use std::collections::BTreeMap;

use crate::error::{Code, ValidationError};
use crate::keyspace::{is_reserved_word, PSEUDO_ROOMS};
use crate::manifest::{
    ControlSpec, DiscoveryMode, FamilyHold, FamilyUntil, ParamSpec, ParamType, UnitKind,
    WidgetKind, WidgetSpec, WriteMode, CAPABILITIES, ENABLED_PARAM, VOCABULARY,
};
use crate::repo::{House, LoadedEntity};

/// Structural validation that does not involve key expansion or grants:
/// uniqueness, capabilities, reserved rooms, ownership, zones, params.
pub fn validate(house: &House) -> Vec<ValidationError> {
    let mut errors = Vec::new();

    check_names(house, &mut errors);
    check_duplicates(house, &mut errors);
    check_manifest_shape(house, &mut errors);
    check_entities(house, &mut errors);
    check_persons(house, &mut errors);
    check_aspect_intervals(house, &mut errors);
    check_zones(house, &mut errors);
    check_params(house, &mut errors);
    check_dashboard(house, &mut errors);

    errors
}

/// Whether a name is usable as one bus key segment. Other names break the
/// fixed key schema (`/`), mean something to the bus (`*`, `$`, `?`, `#`), or
/// risk whitespace and encoding problems. A bad unit name would also panic the
/// supervisor's liveliness subscriber. `.` and `..` are refused as path-like.
/// The SDK refuses the same names, so a name that passes here is one every
/// unit can build keys from.
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
fn check_names(house: &House, errors: &mut Vec<ValidationError>) {
    let segment_message = |what: &str, name: &str| {
        format!(
            "{what} \"{name}\" must be a single key segment (letters, digits, \"_\", \"-\", \".\"; not \".\" or \"..\")"
        )
    };
    for unit in &house.units {
        let name = &unit.manifest.unit.name;
        let file = Some(unit.path.clone());
        if !valid_segment(name) {
            errors.push(ValidationError::new(
                Code::InvalidName,
                name,
                segment_message("unit name", name),
                file.clone(),
            ));
        } else if name == "system" {
            errors.push(ValidationError::new(
                Code::ReservedUnitName,
                name,
                "unit name \"system\" is reserved for the core's meta keys",
                file.clone(),
            ));
        }
        if let Some(params) = &unit.manifest.params {
            for param in params.keys() {
                if !valid_segment(param) {
                    errors.push(ValidationError::new(
                        Code::InvalidName,
                        format!("{name}.{param}"),
                        segment_message("parameter name", param),
                        file.clone(),
                    ));
                }
            }
        }
    }
    for entity in &house.entities {
        let file = Some(entity.path.clone());
        if !valid_segment(&entity.name) {
            errors.push(ValidationError::new(
                Code::InvalidName,
                &entity.name,
                segment_message("entity name", &entity.name),
                file.clone(),
            ));
        }
        for aspect in entity.file.aspects.iter().flat_map(|a| a.keys()) {
            if !valid_segment(aspect) {
                errors.push(ValidationError::new(
                    Code::InvalidName,
                    format!("{}.{aspect}", entity.name),
                    segment_message("aspect name", aspect),
                    file.clone(),
                ));
            }
        }
        let room = &entity.file.entity.room;
        if !valid_segment(room) {
            errors.push(ValidationError::new(
                Code::InvalidName,
                &entity.name,
                segment_message("room", room),
                file,
            ));
        }
    }
    for zone in house.zones.keys() {
        if !valid_segment(zone) {
            errors.push(ValidationError::new(
                Code::InvalidName,
                zone,
                segment_message("zone name", zone),
                Some("zones.toml".to_string()),
            ));
        }
    }
    for view in house.dashboard.iter().flat_map(|d| &d.view) {
        if !valid_segment(&view.name) {
            errors.push(ValidationError::new(
                Code::InvalidName,
                &view.name,
                segment_message("view name", &view.name),
                Some("dashboard.toml".to_string()),
            ));
        }
    }
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
            paths.sort_unstable();
            errors.push(ValidationError::new(
                Code::DuplicateUnitName,
                name,
                format!("defined in {}", paths.join(" and ")),
                None,
            ));
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
            errors.push(ValidationError::new(
                Code::DuplicateEntityName,
                name,
                format!("defined in {}", paths.join(" and ")),
                None,
            ));
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
            errors.push(ValidationError::new(
                Code::DuplicateEntityId,
                id,
                format!("bound to \"{owner}\" by {}", paths.join(" and ")),
                None,
            ));
        }
    }
}

fn check_manifest_shape(house: &House, errors: &mut Vec<ValidationError>) {
    for unit in &house.units {
        let name = &unit.manifest.unit.name;
        let file = Some(unit.path.clone());
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
                errors.push(ValidationError::new(
                    Code::InvalidManifest,
                    name,
                    message,
                    file.clone(),
                ));
            }
        }
        if unit.manifest.unit.kind == UnitKind::Automation {
            let enabled = unit
                .manifest
                .params
                .as_ref()
                .and_then(|p| p.get(ENABLED_PARAM));
            if enabled.is_some_and(|spec| spec.param_type != ParamType::Bool) {
                errors.push(ValidationError::new(
                    Code::InvalidManifest,
                    name,
                    "an automation's \"enabled\" parameter pauses it, so it must be a bool",
                    file.clone(),
                ));
            }
        }
        if unit.manifest.unit.kind == UnitKind::Adapter {
            if unit.manifest.entities.is_none() {
                errors.push(ValidationError::new(
                    Code::InvalidManifest,
                    name,
                    "adapter requires an [entities] section",
                    file.clone(),
                ));
            }
            if unit.manifest.discovery.is_none() {
                errors.push(ValidationError::new(
                    Code::InvalidManifest,
                    name,
                    "adapter requires a [discovery] section",
                    file.clone(),
                ));
            }
        } else {
            // Automations may bind entities (virtual sensors). Services have
            // no need to, and are refused until one does.
            if unit.manifest.entities.is_some() && unit.manifest.unit.kind != UnitKind::Automation {
                errors.push(ValidationError::new(
                    Code::InvalidManifest,
                    name,
                    "[entities] is only valid for adapters and automations",
                    file.clone(),
                ));
            }
            // Services may talk to an external backend (e.g. the recorder's
            // store) and use [discovery] the same way adapters do.
            if unit.manifest.discovery.is_some() && unit.manifest.unit.kind != UnitKind::Service {
                errors.push(ValidationError::new(
                    Code::InvalidManifest,
                    name,
                    "[discovery] is only valid for adapters and services",
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
                Code::UnknownCapability,
                &entity.name,
                format!("unknown capability \"{capability}\""),
                file.clone(),
            ));
        }

        // A write mode governs commands, so it means nothing on a capability
        // without a base aspect. It is required where there is something to
        // govern, so a light or a lock does not silently get a default policy.
        if entity.file.write_policy.mode.is_none()
            && VOCABULARY
                .iter()
                .any(|c| c.name == capability && c.base.is_some())
        {
            errors.push(ValidationError::new(
                Code::WriteModeRequired,
                &entity.name,
                format!(
                    "capability \"{capability}\" takes commands, so [write_policy] needs a mode"
                ),
                file.clone(),
            ));
        }

        check_family_policy(house, entity, capability, file.as_ref(), errors);
        check_actions(house, entity, capability, file.as_ref(), errors);
        check_causes(entity, capability, file.as_ref(), errors);

        // An adapter binds devices, so its entity files address them. An
        // automation's do not: a computed value has no device behind it, and
        // requiring an `id` would only make units invent one.
        let owner_is_adapter = house
            .unit(&entity.file.write_policy.owner)
            .is_some_and(|u| u.manifest.unit.kind == UnitKind::Adapter);
        if owner_is_adapter && entity.file.entity.id.is_none() {
            errors.push(ValidationError::new(
                Code::EntityIdRequired,
                &entity.name,
                "an adapter-owned entity needs an [entity] id: its adapter-native address",
                file.clone(),
            ));
        }

        let room = &entity.file.entity.room;
        if is_reserved_word(room) && !PSEUDO_ROOMS.contains(&room.as_str()) {
            errors.push(ValidationError::new(
                Code::ReservedRoomName,
                &entity.name,
                format!("room \"{room}\" is a reserved word"),
                file.clone(),
            ));
        }

        let owner = &entity.file.write_policy.owner;
        match house.unit(owner) {
            None => errors.push(ValidationError::new(
                Code::MissingOwnerUnit,
                &entity.name,
                format!("owner unit \"{owner}\" does not exist"),
                file.clone(),
            )),
            Some(unit)
                if !matches!(
                    unit.manifest.unit.kind,
                    UnitKind::Adapter | UnitKind::Automation
                ) =>
            {
                errors.push(ValidationError::new(
                    Code::MissingOwnerUnit,
                    &entity.name,
                    format!("owner \"{owner}\" is not an adapter or automation"),
                    file.clone(),
                ));
            }
            Some(_) if owner != &entity.owner => {
                errors.push(ValidationError::new(
                    Code::OwnerMismatch,
                    &entity.name,
                    format!("owner \"{owner}\" but bound by unit \"{}\"", entity.owner),
                    file.clone(),
                ));
            }
            Some(unit)
                if unit.manifest.unit.kind == UnitKind::Automation
                    && entity.file.write_policy.mode() == WriteMode::Arbitrated =>
            {
                // A commandable virtual entity is a latch
                // (docs/design.md#commandable-virtual-entities). There is no
                // device to contend for and no hold to expire, so arbitration
                // has nothing to order.
                errors.push(ValidationError::new(
                    Code::VirtualEntityArbitrated,
                    &entity.name,
                    "automation-owned entities are latches and cannot be arbitrated; use shared or exclusive",
                    file.clone(),
                ));
            }
            Some(_) => {}
        }
    }
}

/// An entity's `[write_policy.family]` (docs/design.md#family-policy): on an
/// arbitrated entity, for aspects its capability has, with an `until` the
/// arbiter can read. A time of day needs the house clock, whose minute
/// gives the arbiter the house's UTC offset.
fn check_family_policy(
    house: &House,
    entity: &LoadedEntity,
    capability: &str,
    file: Option<&String>,
    errors: &mut Vec<ValidationError>,
) {
    let policy = &entity.file.write_policy;
    if policy.family.is_empty() {
        return;
    }
    if policy.mode() != WriteMode::Arbitrated {
        errors.push(ValidationError::new(
            Code::FamilyPolicyNotArbitrated,
            &entity.name,
            format!(
                "[write_policy.family] needs mode \"arbitrated\", not \"{}\"",
                policy.mode()
            ),
            file.cloned(),
        ));
    }
    let clock = has_clock(house);
    let vocabulary = VOCABULARY.iter().find(|c| c.name == capability);
    for (aspect, values) in &policy.family {
        let known = vocabulary.is_some_and(|c| {
            c.base == Some(aspect.as_str()) || c.aspects.contains(&aspect.as_str())
        });
        if !known {
            errors.push(ValidationError::new(
                Code::FamilyPolicyAspect,
                format!("{}.{aspect}", entity.name),
                format!("capability \"{capability}\" has no aspect \"{aspect}\""),
                file.cloned(),
            ));
        }
        for (value, hold) in values {
            if let FamilyHold::Until(FamilyUntil { until }) = hold {
                if until == "next change" {
                    continue;
                }
                if !is_time_of_day(until) {
                    errors.push(ValidationError::new(
                        Code::FamilyPolicyUntil,
                        format!("{}.{aspect}.{value}", entity.name),
                        format!("until \"{until}\" is neither \"HH:MM\" nor \"next change\""),
                        file.cloned(),
                    ));
                } else if !clock {
                    errors.push(ValidationError::new(
                        Code::TimeOfDayNoClock,
                        format!("{}.{aspect}.{value}", entity.name),
                        format!("until \"{until}\" is a time of day, and no unit publishes {CLOCK_MINUTE}"),
                        file.cloned(),
                    ));
                }
            }
        }
    }
}

/// The key whose payload carries the house's local time and UTC offset.
const CLOCK_MINUTE: &str = "home/clock/minute";

/// Whether some unit publishes the house clock's minute.
fn has_clock(house: &House) -> bool {
    house.units.iter().any(|u| {
        u.manifest
            .bus
            .iter()
            .flat_map(|b| b.publishes.values())
            .any(|p| p.key == CLOCK_MINUTE)
    })
}

/// Whether `s` is a time of day, `"HH:MM"` (`parse_time`).
fn is_time_of_day(s: &str) -> bool {
    parse_time(s).is_some()
}

/// An entity's `[causes]` (docs/design.md#device-reported-causes): on an
/// arbitrated entity whose base aspect is `on`.
fn check_causes(
    entity: &LoadedEntity,
    capability: &str,
    file: Option<&String>,
    errors: &mut Vec<ValidationError>,
) {
    if entity.file.causes.is_none() {
        return;
    }
    if entity.file.write_policy.mode() != WriteMode::Arbitrated {
        errors.push(ValidationError::new(
            Code::CausesNotArbitrated,
            &entity.name,
            format!(
                "[causes] needs mode \"arbitrated\", not \"{}\"",
                entity.file.write_policy.mode()
            ),
            file.cloned(),
        ));
    }
    let timed = entity.file.entity.features.iter().any(|f| f == "runtime_s");
    if timed
        && entity
            .file
            .causes
            .as_ref()
            .is_some_and(|c| c.uncommanded.is_some())
    {
        errors.push(ValidationError::new(
            Code::CausesRuntime,
            &entity.name,
            "the device closes on its own after runtime_s, so uncommanded = \"person\" would make each close the family's",
            file.cloned(),
        ));
    }
    let base = VOCABULARY
        .iter()
        .find(|c| c.name == capability)
        .and_then(|c| c.base);
    if base != Some("on") {
        errors.push(ValidationError::new(
            Code::CausesCapability,
            &entity.name,
            format!("capability \"{capability}\" has no base aspect \"on\" to attribute"),
            file.cloned(),
        ));
    }
}

/// The base aspects that are booleans, which a gesture action may toggle.
const TOGGLED: &[&str] = &["on", "locked"];

/// A button's `[actions]` (docs/design.md#gesture-actions): each target
/// exists, each aspect is the target's, each action says one thing to
/// send, a hold goes only to an arbitrated target, and an unlock is
/// declared.
fn check_actions(
    house: &House,
    entity: &LoadedEntity,
    capability: &str,
    file: Option<&String>,
    errors: &mut Vec<ValidationError>,
) {
    let Some(actions) = &entity.file.actions else {
        return;
    };
    if capability != "button" {
        errors.push(ValidationError::new(
            Code::ActionsNotButton,
            &entity.name,
            format!("[actions] needs capability \"button\", not \"{capability}\""),
            file.cloned(),
        ));
    }
    let clock = has_clock(house);
    for (gesture, targets) in actions {
        for (name, action) in targets {
            let subject = format!("{}.{gesture}.{name}", entity.name);
            let err = |code: Code, message: String| {
                ValidationError::new(code, subject.clone(), message, file.cloned())
            };
            let Some(target) = house.entities.iter().find(|e| &e.name == name) else {
                errors.push(err(
                    Code::ActionUnknownTarget,
                    format!("target entity \"{name}\" does not exist"),
                ));
                continue;
            };
            let target_capability = target.file.entity.capability.as_str();
            let vocabulary = VOCABULARY.iter().find(|c| c.name == target_capability);
            let release = action.hold.as_deref() == Some("release");
            let sends = usize::from(action.toggle.is_some()) + usize::from(action.set.is_some());
            let hold_ok = action
                .hold
                .as_deref()
                .is_none_or(|h| h == "none" || h == "release" || is_time_of_day(h));
            if !hold_ok
                || action.set.as_ref().is_some_and(BTreeMap::is_empty)
                || if release { sends != 0 } else { sends != 1 }
            {
                errors.push(err(
                    Code::ActionShape,
                    "an action takes one of toggle and set, or hold = \"release\" alone, and a hold of \"none\", \"release\" or \"HH:MM\"".to_string(),
                ));
                continue;
            }
            // A command goes to the base aspect or to a feature the entity
            // declares, as the dashboard's controls do. A toggle needs a
            // boolean, and a set sends scalars.
            let base = vocabulary.and_then(|c| c.base);
            if base.is_none() {
                errors.push(err(
                    Code::ActionAspect,
                    format!("capability \"{target_capability}\" takes no commands"),
                ));
                continue;
            }
            let commandable = |aspect: &str| {
                base == Some(aspect) || target.file.entity.features.iter().any(|f| f == aspect)
            };
            if let Some(aspect) = &action.toggle {
                if !(base == Some(aspect.as_str()) && TOGGLED.contains(&aspect.as_str())) {
                    errors.push(err(
                        Code::ActionAspect,
                        format!(
                            "\"{aspect}\" of \"{name}\" is not a boolean base aspect to toggle"
                        ),
                    ));
                }
            }
            for (aspect, value) in action.set.iter().flatten() {
                if !commandable(aspect) {
                    errors.push(err(
                        Code::ActionAspect,
                        format!("\"{name}\" has no commandable aspect \"{aspect}\": its base aspect or a declared feature"),
                    ));
                }
                let scalar = match value {
                    toml::Value::Boolean(_) | toml::Value::Integer(_) | toml::Value::String(_) => {
                        true
                    }
                    toml::Value::Float(f) => f.is_finite(),
                    _ => false,
                };
                let typed = !TOGGLED.contains(&aspect.as_str()) || value.is_bool();
                if !scalar || !typed {
                    errors.push(err(
                        Code::ActionShape,
                        format!("set {aspect} = {value} is not a value to send"),
                    ));
                }
            }
            if action.hold.is_some() && target.file.write_policy.mode() != WriteMode::Arbitrated {
                errors.push(err(
                    Code::ActionHoldNotArbitrated,
                    format!("target \"{name}\" is not arbitrated, so the action takes no hold"),
                ));
            }
            if action.hold.as_deref().is_some_and(is_time_of_day) && !clock {
                errors.push(err(
                    Code::TimeOfDayNoClock,
                    format!(
                        "hold \"{}\" is a time of day, and no unit publishes {CLOCK_MINUTE}",
                        action.hold.as_deref().unwrap_or_default()
                    ),
                ));
            }
            // A release sends the hold beneath at once, which may be an unlock.
            let unlocks = target_capability == "lock"
                && (release
                    || action.toggle.as_deref() == Some("locked")
                    || action
                        .set
                        .as_ref()
                        .and_then(|s| s.get("locked"))
                        .is_some_and(|v| v.as_bool() != Some(true)));
            if unlocks && !action.unlocks {
                errors.push(err(
                    Code::ActionUnlocks,
                    format!("the action can unlock \"{name}\"; say unlocks = true"),
                ));
            }
        }
    }
}

/// The aspects a person's source may name: presence evidence (`home`,
/// `sighted`) or a position fix (`lat`, read with its `lon`, `accuracy` and
/// `fixed_at`).
const PERSON_SOURCE_ASPECTS: &[&str] = &["home", "sighted", "lat"];

/// A person is a human computed from the trackers it lists, so an adapter
/// cannot bind one (docs/design.md#map-and-people). An unknown source entity is
/// `source-unknown-entity`, which `grants::resolve_sources` reports.
fn check_persons(house: &House, errors: &mut Vec<ValidationError>) {
    for entity in &house.entities {
        if entity.file.entity.capability != "person" {
            continue;
        }
        let file = Some(entity.path.clone());
        let owner_is_adapter = house
            .unit(&entity.file.write_policy.owner)
            .is_some_and(|u| u.manifest.unit.kind == UnitKind::Adapter);
        if owner_is_adapter {
            errors.push(ValidationError::new(
                Code::PersonBoundByAdapter,
                &entity.name,
                format!(
                    "adapter \"{}\" binds a person; bind the device as `tracker` and list it in a person file owned by an automation",
                    entity.file.write_policy.owner
                ),
                file.clone(),
            ));
        }
        for (name, source) in entity.file.sources.iter().flatten() {
            let subject = format!("{}.{name}", entity.name);
            let Some(src) = house.entities.iter().find(|e| e.name == source.entity) else {
                continue;
            };
            if src.file.entity.capability != "tracker" {
                errors.push(ValidationError::new(
                    Code::PersonSourceNotTracker,
                    subject,
                    format!(
                        "source \"{}\" has capability `{}`; a person's sources are trackers",
                        src.name, src.file.entity.capability
                    ),
                    file.clone(),
                ));
            } else if !PERSON_SOURCE_ASPECTS.contains(&source.aspect.as_str()) {
                errors.push(ValidationError::new(
                    Code::PersonSourceAspect,
                    subject,
                    format!(
                        "aspect \"{}\" is not one of `home`, `sighted` or `lat`",
                        source.aspect
                    ),
                    file.clone(),
                ));
            }
        }
    }
}

/// An interval total weighs each sample by the share of its interval
/// inside a window, so an interval of no length would divide by zero.
fn check_aspect_intervals(house: &House, errors: &mut Vec<ValidationError>) {
    for entity in &house.entities {
        for (aspect, decl) in entity.file.aspects.iter().flatten() {
            if decl.interval_s == 0 {
                errors.push(ValidationError::new(
                    Code::AspectInterval,
                    format!("{}.{aspect}", entity.name),
                    "interval_s is 0; it is the length of each sample's interval in seconds"
                        .to_string(),
                    Some(entity.path.clone()),
                ));
            }
        }
    }
}

fn check_zones(house: &House, errors: &mut Vec<ValidationError>) {
    let rooms = house.rooms();
    let file = Some("zones.toml".to_string());
    for (zone, members) in &house.zones {
        if is_reserved_word(zone) {
            errors.push(ValidationError::new(
                Code::ReservedZoneName,
                zone,
                format!("zone name \"{zone}\" is a reserved word"),
                file.clone(),
            ));
            continue;
        }
        if rooms.contains(&zone.as_str()) {
            errors.push(ValidationError::new(
                Code::ZoneRoomCollision,
                zone,
                format!("zone name \"{zone}\" collides with a room name"),
                file.clone(),
            ));
        }
        for room in members {
            if PSEUDO_ROOMS.contains(&room.as_str()) {
                errors.push(ValidationError::new(
                    Code::ZonePseudoRoom,
                    zone,
                    format!("zone includes pseudo-room \"{room}\""),
                    file.clone(),
                ));
            } else if !rooms.contains(&room.as_str()) {
                errors.push(ValidationError::new(
                    Code::ZoneUnknownRoom,
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
        let Some(params) = &unit.manifest.params else {
            continue;
        };
        for (name, spec) in params {
            let subject = format!("{}.{name}", unit.manifest.unit.name);
            check_param(&subject, spec, &unit.path, errors);
        }
    }
}

fn check_param(subject: &str, spec: &ParamSpec, path: &str, errors: &mut Vec<ValidationError>) {
    let before = errors.len();
    let mut err = |code: Code, message: String| {
        errors.push(ValidationError::new(
            code,
            subject,
            message,
            Some(path.to_string()),
        ));
    };
    let t = spec.param_type;

    let default_ok = match (t, &spec.default) {
        (ParamType::Bool, toml::Value::Boolean(_))
        | (ParamType::Int, toml::Value::Integer(_))
        | (ParamType::Float, toml::Value::Float(_) | toml::Value::Integer(_))
        | (ParamType::String, toml::Value::String(_)) => true,
        (ParamType::Time, toml::Value::String(s)) => parse_time(s).is_some(),
        _ => false,
    };
    if !default_ok {
        err(
            Code::InvalidDefault,
            format!(
                "default {} does not match type \"{t}\"",
                display_value(&spec.default)
            ),
        );
    }

    let Some(constraint) = &spec.constraint else {
        return;
    };
    for (key, value) in constraint {
        let valid_for_type = match key.as_str() {
            "min" | "max" => matches!(t, ParamType::Int | ParamType::Float),
            "after" | "before" => t == ParamType::Time,
            "enum" => t == ParamType::String,
            _ => {
                err(
                    Code::MalformedConstraint,
                    format!("unknown constraint \"{key}\""),
                );
                continue;
            }
        };
        if !valid_for_type {
            err(
                Code::MalformedConstraint,
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
                    if !items.is_empty() && items.iter().all(toml::Value::is_str)
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
                Code::MalformedConstraint,
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
                Code::MalformedConstraint,
                format!(
                    "min ({}) is greater than max ({})",
                    display_value(&constraint["min"]),
                    display_value(&constraint["max"])
                ),
            );
        }
    }

    // The default must satisfy its own constraint. The repo-edit parameter
    // path is enforced here, so an out-of-constraint default does not plan and
    // cannot reach a running unit. Skipped when this param already has errors,
    // since judging a default against a malformed constraint only adds noise.
    // `errors.len() == before` also implies the default matched its type.
    if errors.len() == before {
        if let Err(message) = crate::config::default_within_constraint(spec) {
            errors.push(ValidationError::new(
                Code::InvalidDefault,
                subject,
                format!("default {}: {message}", display_value(&spec.default)),
                Some(path.to_string()),
            ));
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

/// Parses a time of day: `"HH:MM"` on a 24-hour clock, two ASCII digits
/// each side. The SDK reads a `time` parameter with Python's
/// `time.fromisoformat`, which refuses `7:30` and `+7:30`, so the core
/// refuses them too. `tests/fixtures/time_of_day.json` holds the cases
/// every reader of the grammar is tested against.
pub(crate) fn parse_time(s: &str) -> Option<(u8, u8)> {
    let b = s.as_bytes();
    if b.len() != 5 || b[2] != b':' || ![0, 1, 3, 4].iter().all(|&i| b[i].is_ascii_digit()) {
        return None;
    }
    let hh = (b[0] - b'0') * 10 + (b[1] - b'0');
    let mm = (b[3] - b'0') * 10 + (b[4] - b'0');
    (hh < 24 && mm < 60).then_some((hh, mm))
}

/// `dashboard.toml`: each view is either a generated one or a composition.
/// Each widget carries the fields its kind takes and no others, and every
/// reference resolves against the house.
fn check_dashboard(house: &House, errors: &mut Vec<ValidationError>) {
    let Some(dashboard) = &house.dashboard else {
        return;
    };
    let file = Some("dashboard.toml".to_string());
    let rooms = house.rooms();
    let mut seen: Vec<&str> = Vec::new();
    for view in &dashboard.view {
        if matches!(view.name.as_str(), "health" | "notshown") {
            errors.push(ValidationError::new(
                Code::DashboardReservedView,
                &view.name,
                format!(
                    "view name \"{}\" belongs to the dashboard's fixed chrome",
                    view.name
                ),
                file.clone(),
            ));
        }
        if seen.contains(&view.name.as_str()) {
            errors.push(ValidationError::new(
                Code::DashboardDuplicateView,
                &view.name,
                format!("view \"{}\" is declared more than once", view.name),
                file.clone(),
            ));
        }
        seen.push(&view.name);
        if view.kind.is_some() == !view.widgets.is_empty() {
            errors.push(ValidationError::new(
                Code::DashboardViewShape,
                &view.name,
                "a view is a generated `kind` or a list of `widgets`, never both or neither",
                file.clone(),
            ));
        }
        for (i, widget) in view.widgets.iter().enumerate() {
            let subject = format!("{}[{i}]", view.name);
            check_widget(house, &rooms, widget, &subject, file.as_deref(), errors);
            // A group's members are widgets like any other and get the same
            // checks. Groups go one level deep, because the file is not meant
            // to be a layout language.
            for (j, member) in widget.widgets.iter().enumerate() {
                let subject = format!("{subject}[{j}]");
                if member.kind == WidgetKind::Group {
                    errors.push(ValidationError::new(
                        Code::DashboardNestedGroup,
                        &subject,
                        "a group holds widgets, never another group",
                        file.clone(),
                    ));
                    continue;
                }
                check_widget(house, &rooms, member, &subject, file.as_deref(), errors);
            }
        }
    }
    for (i, control) in dashboard.control.iter().enumerate() {
        check_control(
            house,
            control,
            &format!("control[{i}]"),
            file.as_deref(),
            errors,
        );
    }
}

/// One `[[control]]`: one target, a reference that resolves, and a positive
/// finite step. The dashboard reads this wherever it draws that control. The
/// core only checks that it names something real.
fn check_control(
    house: &House,
    control: &ControlSpec,
    subject: &str,
    file: Option<&str>,
    errors: &mut Vec<ValidationError>,
) {
    let entity_target = control.entity.is_some() || control.aspect.is_some();
    let param_target = control.unit.is_some() || control.param.is_some();
    let complete = match (entity_target, param_target) {
        (true, false) => control.entity.is_some() && control.aspect.is_some(),
        (false, true) => control.unit.is_some() && control.param.is_some(),
        _ => false,
    };
    if !complete {
        errors.push(ValidationError::new(
            Code::DashboardControlTarget,
            subject,
            "a control names one target: `entity` with `aspect`, or `unit` with `param`",
            file.map(str::to_string),
        ));
        return;
    }
    if !(control.step.is_finite() && control.step > 0.0) {
        errors.push(ValidationError::new(
            Code::DashboardControlStep,
            subject,
            format!("`step` must be a positive number, not {}", control.step),
            file.map(str::to_string),
        ));
    }
    if let (Some(entity), Some(aspect)) = (&control.entity, &control.aspect) {
        if !house.entities.iter().any(|e| &e.name == entity) {
            errors.push(ValidationError::new(
                Code::DashboardUnknownEntity,
                subject,
                format!("control names unknown entity \"{entity}\""),
                file.map(str::to_string),
            ));
        }
        if !valid_segment(aspect) {
            errors.push(ValidationError::new(
                Code::DashboardInvalidAspect,
                subject,
                format!("aspect \"{aspect}\" must be a single key segment"),
                file.map(str::to_string),
            ));
        }
    }
    if let (Some(unit), Some(param)) = (&control.unit, &control.param) {
        match house.unit(unit) {
            None => errors.push(ValidationError::new(
                Code::DashboardUnknownUnit,
                subject,
                format!("control names unknown unit \"{unit}\""),
                file.map(str::to_string),
            )),
            // A step sets the grain of a control, so it must name a
            // parameter the unit declares.
            Some(u)
                if !u
                    .manifest
                    .params
                    .as_ref()
                    .is_some_and(|p| p.contains_key(param)) =>
            {
                errors.push(ValidationError::new(
                    Code::DashboardUnknownParam,
                    subject,
                    format!("control names unknown parameter \"{param}\" on unit \"{unit}\""),
                    file.map(str::to_string),
                ));
            }
            Some(_) => {}
        }
    }
}

/// One widget: the fields its kind takes, and references that resolve
/// against the house.
fn check_widget(
    house: &House,
    rooms: &[&str],
    widget: &WidgetSpec,
    subject: &str,
    file: Option<&str>,
    errors: &mut Vec<ValidationError>,
) {
    if let Some(message) = widget_fields_message(widget) {
        errors.push(ValidationError::new(
            Code::DashboardWidgetFields,
            subject,
            message,
            file.map(str::to_string),
        ));
        return;
    }
    if let Some(entity) = &widget.entity {
        match house.entities.iter().find(|e| &e.name == entity) {
            None => errors.push(ValidationError::new(
                Code::DashboardUnknownEntity,
                subject,
                format!("widget names unknown entity \"{entity}\""),
                file.map(str::to_string),
            )),
            // A capability widget draws that capability's vocabulary, so it
            // only makes sense over an entity of that capability.
            Some(e)
                if widget.kind == WidgetKind::Burner && e.file.entity.capability != "burner" =>
            {
                errors.push(ValidationError::new(
                    Code::DashboardWidgetCapability,
                    subject,
                    format!(
                        "a `burner` widget needs a burner; \"{entity}\" is a {}",
                        e.file.entity.capability
                    ),
                    file.map(str::to_string),
                ));
            }
            Some(_) => {}
        }
    }
    // A map colours what it draws: people, and trackers.
    for entity in widget.colors.keys() {
        match house.entities.iter().find(|e| &e.name == entity) {
            None => errors.push(ValidationError::new(
                Code::DashboardUnknownEntity,
                subject,
                format!("`colors` names unknown entity \"{entity}\""),
                file.map(str::to_string),
            )),
            Some(e) if !matches!(e.file.entity.capability.as_str(), "person" | "tracker") => {
                errors.push(ValidationError::new(
                    Code::DashboardWidgetCapability,
                    subject,
                    format!(
                        "a `map` colours a person or a tracker; \"{entity}\" is a {}",
                        e.file.entity.capability
                    ),
                    file.map(str::to_string),
                ));
            }
            Some(_) => {}
        }
    }
    if let Some(aspect) = &widget.aspect {
        if !valid_segment(aspect) {
            errors.push(ValidationError::new(
                Code::DashboardInvalidAspect,
                subject,
                format!("aspect \"{aspect}\" must be a single key segment"),
                file.map(str::to_string),
            ));
        }
    }
    if let Some(room) = &widget.room {
        if !rooms.contains(&room.as_str()) {
            errors.push(ValidationError::new(
                Code::DashboardUnknownRoom,
                subject,
                format!("widget names unknown room \"{room}\""),
                file.map(str::to_string),
            ));
        }
    }
    if let Some(unit) = &widget.unit {
        if house.unit(unit).is_none() {
            errors.push(ValidationError::new(
                Code::DashboardUnknownUnit,
                subject,
                format!("widget names unknown unit \"{unit}\""),
                file.map(str::to_string),
            ));
        }
    }
}

/// The fields a widget kind takes (required ones first, then optional) against
/// the fields it carries. None when they agree.
fn widget_fields_message(widget: &WidgetSpec) -> Option<String> {
    let (required, optional): (&[&str], &[&str]) = match widget.kind {
        WidgetKind::Tile | WidgetKind::Dial => (&["entity"], &["aspect"]),
        WidgetKind::Burner | WidgetKind::Entity => (&["entity"], &[]),
        WidgetKind::Chart => (&["entity", "aspect"], &["hours"]),
        WidgetKind::Room => (&["room"], &[]),
        WidgetKind::Unit | WidgetKind::Params => (&["unit"], &[]),
        WidgetKind::Map => (&[], &["hours", "colors"]),
        WidgetKind::People | WidgetKind::Deviations => (&[], &[]),
        WidgetKind::Group => (&["widgets"], &["label"]),
    };
    let present: Vec<&str> = [
        ("entity", widget.entity.is_some()),
        ("aspect", widget.aspect.is_some()),
        ("room", widget.room.is_some()),
        ("unit", widget.unit.is_some()),
        ("hours", widget.hours.is_some()),
        ("label", widget.label.is_some()),
        ("widgets", !widget.widgets.is_empty()),
        ("colors", !widget.colors.is_empty()),
    ]
    .into_iter()
    .filter(|(_, set)| *set)
    .map(|(name, _)| name)
    .collect();
    let kind = format!("{:?}", widget.kind).to_lowercase();
    for field in required {
        if !present.contains(field) {
            return Some(format!("a `{kind}` widget needs `{field}`"));
        }
    }
    for field in &present {
        if !required.contains(field) && !optional.contains(field) {
            return Some(format!("a `{kind}` widget takes no `{field}`"));
        }
    }
    if let Some(hours) = widget.hours {
        if !(hours > 0.0 && hours.is_finite()) {
            return Some(format!("`hours` must be a positive number, not {hours}"));
        }
    }
    // The colour is written into the map's markup, so only a hex colour
    // passes.
    for (entity, color) in &widget.colors {
        if !hex_color(color) {
            return Some(format!(
                "`colors.{entity}` must be a hex colour like \"#d9534f\", not \"{color}\""
            ));
        }
    }
    None
}

/// `#rgb` or `#rrggbb`.
fn hex_color(color: &str) -> bool {
    color
        .strip_prefix('#')
        .is_some_and(|hex| matches!(hex.len(), 3 | 6) && hex.chars().all(|c| c.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use super::parse_time;

    /// The core reads a time of day as every other reader does
    /// (`tests/fixtures/time_of_day.json`).
    #[test]
    fn the_time_of_day_grammar_is_the_shared_one() {
        let cases: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/time_of_day.json")).unwrap();
        for s in cases["valid"].as_array().unwrap() {
            let s = s.as_str().unwrap();
            assert!(parse_time(s).is_some(), "{s:?} is a time of day");
        }
        for s in cases["invalid"].as_array().unwrap() {
            let s = s.as_str().unwrap();
            assert!(parse_time(s).is_none(), "{s:?} is not a time of day");
        }
        assert_eq!(parse_time("07:30"), Some((7, 30)));
    }
}
