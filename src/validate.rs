//! Structural validation of a loaded house: names, uniqueness, manifest
//! shape, entity ownership, zones, parameters and `dashboard.toml`.
//! Checks over expanded keys or the grant table live in `expand` and
//! `grants`. The exception is whether some unit publishes the house clock,
//! which the time-of-day checks here need.

use std::collections::BTreeMap;

use crate::error::{Code, Diagnostics, Scoped};
use crate::expand::{self, ExpandedKey};
use crate::keyspace::{is_reserved_word, PSEUDO_ROOMS};
use crate::manifest::{
    self, ControlSpec, DiscoveryMode, FamilyHold, FamilyUntil, ParamType, UnitKind, ValueType,
    WidgetKind, WidgetSpec, WriteMode, ENABLED_PARAM,
};
use crate::params::{parse_time, Constraint};
use crate::repo::{House, LoadedEntity};

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

/// `clock` says whether some unit publishes the house clock's minute.
fn check_entities(house: &House, clock: bool, diags: &mut Diagnostics) {
    for entity in &house.entities {
        let mut diags = diags.in_file(&entity.path);
        let capability = &entity.file.entity.capability;
        if manifest::capability(capability).is_none() {
            diags.error(
                Code::UnknownCapability,
                &entity.name,
                format!("unknown capability \"{capability}\""),
            );
        }

        // A write mode governs commands, so it means nothing on a capability
        // without a base aspect. It is required where there is something to
        // govern, so a light or a lock does not silently get a default policy.
        if entity.file.write_policy.mode.is_none()
            && manifest::capability(capability).is_some_and(|c| c.base.is_some())
        {
            diags.error(
                Code::WriteModeRequired,
                &entity.name,
                format!(
                    "capability \"{capability}\" takes commands, so [write_policy] needs a mode"
                ),
            );
        }

        check_family_policy(entity, capability, clock, &mut diags);
        check_actions(house, entity, capability, clock, &mut diags);
        check_causes(entity, capability, &mut diags);

        // An adapter binds devices, so its entity files address them. An
        // automation's do not: a computed value has no device behind it, and
        // requiring an `id` would only make units invent one.
        let owner_is_adapter = house
            .unit(&entity.file.write_policy.owner)
            .is_some_and(|u| u.manifest.unit.kind == UnitKind::Adapter);
        if owner_is_adapter && entity.file.entity.id.is_none() {
            diags.error(
                Code::EntityIdRequired,
                &entity.name,
                "an adapter-owned entity needs an [entity] id: its adapter-native address",
            );
        }

        let room = &entity.file.entity.room;
        if is_reserved_word(room) && !PSEUDO_ROOMS.contains(&room.as_str()) {
            diags.error(
                Code::ReservedRoomName,
                &entity.name,
                format!("room \"{room}\" is a reserved word"),
            );
        }

        let owner = &entity.file.write_policy.owner;
        match house.unit(owner) {
            None => diags.error(
                Code::MissingOwnerUnit,
                &entity.name,
                format!("owner unit \"{owner}\" does not exist"),
            ),
            Some(unit)
                if !matches!(
                    unit.manifest.unit.kind,
                    UnitKind::Adapter | UnitKind::Automation
                ) =>
            {
                diags.error(
                    Code::MissingOwnerUnit,
                    &entity.name,
                    format!("owner \"{owner}\" is not an adapter or automation"),
                );
            }
            Some(_) if owner != &entity.owner => {
                diags.error(
                    Code::OwnerMismatch,
                    &entity.name,
                    format!("owner \"{owner}\" but bound by unit \"{}\"", entity.owner),
                );
            }
            Some(unit)
                if unit.manifest.unit.kind == UnitKind::Automation
                    && entity.file.write_policy.mode() == WriteMode::Arbitrated =>
            {
                // A commandable virtual entity is a latch
                // (docs/design.md#commandable-virtual-entities). There is no
                // device to contend for and no hold to expire, so arbitration
                // has nothing to order.
                diags.error(
                    Code::VirtualEntityArbitrated,
                    &entity.name,
                    "automation-owned entities are latches and cannot be arbitrated; use shared or exclusive",
                );
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
    entity: &LoadedEntity,
    capability: &str,
    clock: bool,
    diags: &mut Scoped<'_>,
) {
    let policy = &entity.file.write_policy;
    if policy.family.is_empty() {
        return;
    }
    if policy.mode() != WriteMode::Arbitrated {
        diags.error(
            Code::FamilyPolicyNotArbitrated,
            &entity.name,
            format!(
                "[write_policy.family] needs mode \"arbitrated\", not \"{}\"",
                policy.mode()
            ),
        );
    }
    let vocabulary = manifest::capability(capability);
    for (aspect, values) in &policy.family {
        let known = vocabulary.is_some_and(|c| {
            c.base == Some(aspect.as_str()) || c.aspects.contains(&aspect.as_str())
        });
        if !known {
            diags.error(
                Code::FamilyPolicyAspect,
                format!("{}.{aspect}", entity.name),
                format!("capability \"{capability}\" has no aspect \"{aspect}\""),
            );
        }
        for (value, hold) in values {
            if let FamilyHold::Until(FamilyUntil { until }) = hold {
                if until == "next change" {
                    continue;
                }
                if !is_time_of_day(until) {
                    diags.error(
                        Code::FamilyPolicyUntil,
                        format!("{}.{aspect}.{value}", entity.name),
                        format!("until \"{until}\" is neither \"HH:MM\" nor \"next change\""),
                    );
                } else if !clock {
                    diags.error(
                        Code::TimeOfDayNoClock,
                        format!("{}.{aspect}.{value}", entity.name),
                        format!("until \"{until}\" is a time of day, and no unit publishes {CLOCK_MINUTE}"),
                    );
                }
            }
        }
    }
}

/// The key whose payload carries the house's local time and UTC offset.
const CLOCK_MINUTE: &str = "home/clock/minute";

/// Whether some unit publishes the house clock's minute, literally or
/// through a wildcard such as `home/clock/*`. The key itself must match:
/// a publish only under it, such as `home/clock/minute/x`, never carries it.
fn has_clock(expanded: &[ExpandedKey]) -> bool {
    let minute: Vec<&str> = CLOCK_MINUTE.split('/').collect();
    expanded.iter().any(|k| {
        k.direction == expand::Direction::Publishes
            && k.exprs.iter().any(|e| e.matches_key(&minute))
    })
}

/// Whether `s` is a time of day, `"HH:MM"` (`parse_time`).
fn is_time_of_day(s: &str) -> bool {
    parse_time(s).is_some()
}

/// An entity's `[causes]` (docs/design.md#device-reported-causes): on an
/// arbitrated entity whose base aspect is `on`.
fn check_causes(entity: &LoadedEntity, capability: &str, diags: &mut Scoped<'_>) {
    if entity.file.causes.is_none() {
        return;
    }
    if entity.file.write_policy.mode() != WriteMode::Arbitrated {
        diags.error(
            Code::CausesNotArbitrated,
            &entity.name,
            format!(
                "[causes] needs mode \"arbitrated\", not \"{}\"",
                entity.file.write_policy.mode()
            ),
        );
    }
    let timed = entity.file.entity.features.iter().any(|f| f == "runtime_s");
    if timed
        && entity
            .file
            .causes
            .as_ref()
            .is_some_and(|c| c.uncommanded.is_some())
    {
        diags.error(
            Code::CausesRuntime,
            &entity.name,
            "the device closes on its own after runtime_s, so uncommanded = \"person\" would make each close the family's",
        );
    }
    let base = manifest::capability(capability).and_then(|c| c.base);
    if base != Some("on") {
        diags.error(
            Code::CausesCapability,
            &entity.name,
            format!("capability \"{capability}\" has no base aspect \"on\" to attribute"),
        );
    }
}

/// A button's `[actions]` (docs/design.md#gesture-actions): each target
/// exists, each aspect takes a command on the target with a value of its
/// type, each action says one thing to send, a hold goes only to an
/// arbitrated target, and an unlock is declared.
fn check_actions(
    house: &House,
    entity: &LoadedEntity,
    capability: &str,
    clock: bool,
    diags: &mut Scoped<'_>,
) {
    let Some(actions) = &entity.file.actions else {
        return;
    };
    if capability != "button" {
        diags.error(
            Code::ActionsNotButton,
            &entity.name,
            format!("[actions] needs capability \"button\", not \"{capability}\""),
        );
    }
    for (gesture, targets) in actions {
        for (name, action) in targets {
            let subject = format!("{}.{gesture}.{name}", entity.name);
            let Some(target) = house.entities.iter().find(|e| &e.name == name) else {
                diags.error(
                    Code::ActionUnknownTarget,
                    &subject,
                    format!("target entity \"{name}\" does not exist"),
                );
                continue;
            };
            let target_capability = target.file.entity.capability.as_str();
            let vocabulary = manifest::capability(target_capability);
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
                diags.error(
                    Code::ActionShape,
                    &subject,
                    "an action takes one of toggle and set, or hold = \"release\" alone, and a hold of \"none\", \"release\" or \"HH:MM\"".to_string(),
                );
                continue;
            }
            // A command goes to an aspect in the capability's commands
            // column: the base aspect, or a feature the entity declares. A
            // toggle needs a bool command, and a set sends a value of the
            // command's type. A declared feature the vocabulary does not
            // name is an adapter's own knob, which its descriptor types and
            // bounds, so plan takes any scalar for it, as the dashboard
            // does. A feature the vocabulary names as a reading takes none.
            let Some(vocabulary) = vocabulary.filter(|c| !c.commands.is_empty()) else {
                diags.error(
                    Code::ActionAspect,
                    &subject,
                    format!("capability \"{target_capability}\" takes no commands"),
                );
                continue;
            };
            let declared = |aspect: &str| {
                vocabulary.base == Some(aspect)
                    || target.file.entity.features.iter().any(|f| f == aspect)
            };
            let command = |aspect: &str| -> Option<Option<ValueType>> {
                if !declared(aspect) {
                    return None;
                }
                if let Some(&(_, ty)) = vocabulary.commands.iter().find(|(a, _)| *a == aspect) {
                    return Some(Some(ty));
                }
                let named = vocabulary.base == Some(aspect) || vocabulary.aspects.contains(&aspect);
                (!named).then_some(None)
            };
            if let Some(aspect) = &action.toggle {
                if command(aspect) != Some(Some(ValueType::Bool)) {
                    diags.error(
                        Code::ActionAspect,
                        &subject,
                        format!("\"{aspect}\" of \"{name}\" is not a bool command to toggle"),
                    );
                }
            }
            for (aspect, value) in action.set.iter().flatten() {
                let Some(ty) = command(aspect) else {
                    diags.error(
                        Code::ActionAspect,
                        &subject,
                        format!("\"{name}\" takes no command on \"{aspect}\", which is not its base aspect or a declared feature that takes commands"),
                    );
                    continue;
                };
                let typed = match (ty, value) {
                    (Some(ValueType::Bool) | None, toml::Value::Boolean(_))
                    | (Some(ValueType::Number) | None, toml::Value::Integer(_))
                    | (Some(ValueType::Text) | None, toml::Value::String(_)) => true,
                    (Some(ValueType::Number) | None, toml::Value::Float(f)) => f.is_finite(),
                    _ => false,
                };
                if !typed {
                    let what = ty.map_or("scalar".to_string(), |ty| ty.to_string());
                    diags.error(
                        Code::ActionShape,
                        &subject,
                        format!("set {aspect} = {value} is not a {what}"),
                    );
                } else if value.as_str().is_some_and(|text| text.trim().is_empty()) {
                    // Notifiers and the dashboard refuse blank text.
                    diags.error(
                        Code::ActionShape,
                        &subject,
                        format!("set {aspect} is blank text"),
                    );
                }
            }
            if action.hold.is_some() && target.file.write_policy.mode() != WriteMode::Arbitrated {
                diags.error(
                    Code::ActionHoldNotArbitrated,
                    &subject,
                    format!("target \"{name}\" is not arbitrated, so the action takes no hold"),
                );
            }
            if action.hold.as_deref().is_some_and(is_time_of_day) && !clock {
                diags.error(
                    Code::TimeOfDayNoClock,
                    &subject,
                    format!(
                        "hold \"{}\" is a time of day, and no unit publishes {CLOCK_MINUTE}",
                        action.hold.as_deref().unwrap_or_default()
                    ),
                );
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
                diags.error(
                    Code::ActionUnlocks,
                    &subject,
                    format!("the action can unlock \"{name}\"; say unlocks = true"),
                );
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
fn check_persons(house: &House, diags: &mut Diagnostics) {
    for entity in &house.entities {
        if entity.file.entity.capability != "person" {
            continue;
        }
        let mut diags = diags.in_file(&entity.path);
        let owner_is_adapter = house
            .unit(&entity.file.write_policy.owner)
            .is_some_and(|u| u.manifest.unit.kind == UnitKind::Adapter);
        if owner_is_adapter {
            diags.error(
                Code::PersonBoundByAdapter,
                &entity.name,
                format!(
                    "adapter \"{}\" binds a person; bind the device as `tracker` and list it in a person file owned by an automation",
                    entity.file.write_policy.owner
                ),
            );
        }
        for (name, source) in entity.file.sources.iter().flatten() {
            let subject = format!("{}.{name}", entity.name);
            let Some(src) = house.entities.iter().find(|e| e.name == source.entity) else {
                continue;
            };
            if src.file.entity.capability != "tracker" {
                diags.error(
                    Code::PersonSourceNotTracker,
                    subject,
                    format!(
                        "source \"{}\" has capability `{}`; a person's sources are trackers",
                        src.name, src.file.entity.capability
                    ),
                );
            } else if !PERSON_SOURCE_ASPECTS.contains(&source.aspect.as_str()) {
                diags.error(
                    Code::PersonSourceAspect,
                    subject,
                    format!(
                        "aspect \"{}\" is not one of `home`, `sighted` or `lat`",
                        source.aspect
                    ),
                );
            }
        }
    }
}

/// An interval total weighs each sample by the share of its interval
/// inside a window, so an interval of no length would divide by zero.
fn check_aspect_intervals(house: &House, diags: &mut Diagnostics) {
    for entity in &house.entities {
        for (aspect, decl) in entity.file.aspects.iter().flatten() {
            if decl.interval_s == 0 {
                diags.in_file(&entity.path).error(
                    Code::AspectInterval,
                    format!("{}.{aspect}", entity.name),
                    "interval_s is 0; it is the length of each sample's interval in seconds"
                        .to_string(),
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

/// `dashboard.toml`: each view is either a generated one or a composition.
/// Each widget carries the fields its kind takes and no others, and every
/// reference resolves against the house.
fn check_dashboard(house: &House, diags: &mut Diagnostics) {
    let Some(dashboard) = &house.dashboard else {
        return;
    };
    let mut diags = diags.in_file("dashboard.toml");
    let rooms = house.rooms();
    let mut seen: Vec<&str> = Vec::new();
    for view in &dashboard.view {
        if matches!(view.name.as_str(), "health" | "notshown") {
            diags.error(
                Code::DashboardReservedView,
                &view.name,
                format!(
                    "view name \"{}\" belongs to the dashboard's fixed chrome",
                    view.name
                ),
            );
        }
        if seen.contains(&view.name.as_str()) {
            diags.error(
                Code::DashboardDuplicateView,
                &view.name,
                format!("view \"{}\" is declared more than once", view.name),
            );
        }
        seen.push(&view.name);
        if view.kind.is_some() == !view.widgets.is_empty() {
            diags.error(
                Code::DashboardViewShape,
                &view.name,
                "a view is a generated `kind` or a list of `widgets`, never both or neither",
            );
        }
        for (i, widget) in view.widgets.iter().enumerate() {
            let subject = format!("{}[{i}]", view.name);
            check_widget(house, &rooms, widget, &subject, &mut diags);
            // A group's members are widgets like any other and get the same
            // checks. Groups go one level deep, because the file is not meant
            // to be a layout language.
            for (j, member) in widget.widgets.iter().enumerate() {
                let subject = format!("{subject}[{j}]");
                if member.kind == WidgetKind::Group {
                    diags.error(
                        Code::DashboardNestedGroup,
                        &subject,
                        "a group holds widgets, never another group",
                    );
                    continue;
                }
                check_widget(house, &rooms, member, &subject, &mut diags);
            }
        }
    }
    for (i, control) in dashboard.control.iter().enumerate() {
        check_control(house, control, &format!("control[{i}]"), &mut diags);
    }
}

/// One `[[control]]`: one target, a reference that resolves, and a positive
/// finite step. The dashboard reads this wherever it draws that control. The
/// core only checks that it names something real.
fn check_control(house: &House, control: &ControlSpec, subject: &str, diags: &mut Scoped<'_>) {
    let entity_target = control.entity.is_some() || control.aspect.is_some();
    let param_target = control.unit.is_some() || control.param.is_some();
    let complete = match (entity_target, param_target) {
        (true, false) => control.entity.is_some() && control.aspect.is_some(),
        (false, true) => control.unit.is_some() && control.param.is_some(),
        _ => false,
    };
    if !complete {
        diags.error(
            Code::DashboardControlTarget,
            subject,
            "a control names one target: `entity` with `aspect`, or `unit` with `param`",
        );
        return;
    }
    if !(control.step.is_finite() && control.step > 0.0) {
        diags.error(
            Code::DashboardControlStep,
            subject,
            format!("`step` must be a positive number, not {}", control.step),
        );
    }
    if let (Some(entity), Some(aspect)) = (&control.entity, &control.aspect) {
        if !house.entities.iter().any(|e| &e.name == entity) {
            diags.error(
                Code::DashboardUnknownEntity,
                subject,
                format!("control names unknown entity \"{entity}\""),
            );
        }
        if !valid_segment(aspect) {
            diags.error(
                Code::DashboardInvalidAspect,
                subject,
                format!("aspect \"{aspect}\" must be a single key segment"),
            );
        }
    }
    if let (Some(unit), Some(param)) = (&control.unit, &control.param) {
        match house.unit(unit) {
            None => diags.error(
                Code::DashboardUnknownUnit,
                subject,
                format!("control names unknown unit \"{unit}\""),
            ),
            // A step sets the grain of a control, so it must name a
            // parameter the unit declares.
            Some(u)
                if !u
                    .manifest
                    .params
                    .as_ref()
                    .is_some_and(|p| p.contains_key(param)) =>
            {
                diags.error(
                    Code::DashboardUnknownParam,
                    subject,
                    format!("control names unknown parameter \"{param}\" on unit \"{unit}\""),
                );
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
    diags: &mut Scoped<'_>,
) {
    if let Some(message) = widget_fields_message(widget) {
        diags.error(Code::DashboardWidgetFields, subject, message);
        return;
    }
    if let Some(entity) = &widget.entity {
        match house.entities.iter().find(|e| &e.name == entity) {
            None => diags.error(
                Code::DashboardUnknownEntity,
                subject,
                format!("widget names unknown entity \"{entity}\""),
            ),
            // A capability widget draws that capability's vocabulary, so it
            // only makes sense over an entity of that capability.
            Some(e)
                if widget.kind == WidgetKind::Burner && e.file.entity.capability != "burner" =>
            {
                diags.error(
                    Code::DashboardWidgetCapability,
                    subject,
                    format!(
                        "a `burner` widget needs a burner; \"{entity}\" is a {}",
                        e.file.entity.capability
                    ),
                );
            }
            Some(_) => {}
        }
    }
    // A map colours what it draws: people, and trackers.
    for entity in widget.colors.keys() {
        match house.entities.iter().find(|e| &e.name == entity) {
            None => diags.error(
                Code::DashboardUnknownEntity,
                subject,
                format!("`colors` names unknown entity \"{entity}\""),
            ),
            Some(e) if !matches!(e.file.entity.capability.as_str(), "person" | "tracker") => {
                diags.error(
                    Code::DashboardWidgetCapability,
                    subject,
                    format!(
                        "a `map` colours a person or a tracker; \"{entity}\" is a {}",
                        e.file.entity.capability
                    ),
                );
            }
            Some(_) => {}
        }
    }
    if let Some(aspect) = &widget.aspect {
        if !valid_segment(aspect) {
            diags.error(
                Code::DashboardInvalidAspect,
                subject,
                format!("aspect \"{aspect}\" must be a single key segment"),
            );
        }
    }
    if let Some(room) = &widget.room {
        if !rooms.contains(&room.as_str()) {
            diags.error(
                Code::DashboardUnknownRoom,
                subject,
                format!("widget names unknown room \"{room}\""),
            );
        }
    }
    if let Some(unit) = &widget.unit {
        if house.unit(unit).is_none() {
            diags.error(
                Code::DashboardUnknownUnit,
                subject,
                format!("widget names unknown unit \"{unit}\""),
            );
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
    use super::{valid_segment, validate};
    use crate::error::{Code, Diagnostics};
    use crate::expand::expand;
    use crate::repo::House;

    /// A clock that publishes the minute through a wildcard is the house's
    /// clock, so a time-of-day hold is accepted. One that publishes only
    /// the date is not.
    #[test]
    fn a_clock_publishing_the_minute_through_a_wildcard_counts() {
        let zigbee = r#"
            schema = 1

            [unit]
            name = "zigbee"
            kind = "adapter"

            [runtime]
            command = "true"
            restart = "always"

            [discovery]
            mode = "static"
            endpoint = "mqtt://localhost:1883"

            [entities]
            dir = "entities/zigbee/"
        "#;
        let lamp = r#"
            schema = 1

            [entity]
            id = "0x02"
            capability = "light"
            room = "hallway"

            [write_policy]
            mode = "arbitrated"
            owner = "zigbee"

            [write_policy.family.on]
            false = { until = "06:00" }
        "#;
        for (key, has_clock) in [
            ("home/clock/minute", true),
            ("home/clock/*", true),
            ("home/clock/**", true),
            ("home/clock/date", false),
            ("home/clock/minute/x", false),
            ("home/clock/*/x", false),
            ("home/clock/**/tick", false),
        ] {
            let clock = format!(
                r#"
                schema = 1

                [unit]
                name = "clock"
                kind = "service"

                [runtime]
                command = "true"
                restart = "always"

                [bus.publishes]
                time = {{ key = "{key}" }}
                "#
            );
            let house = House::from_toml(
                &[("zigbee", zigbee), ("clock", &clock)],
                &[("zigbee", "lamp", lamp)],
            );
            let mut diags = Diagnostics::default();
            let expanded = expand(&house, &mut diags);
            assert!(diags.errors.is_empty(), "{:?}", diags.errors);
            validate(&house, &expanded, &mut diags);
            let codes: Vec<Code> = diags.errors.iter().map(|e| e.code).collect();
            let expected = if has_clock {
                vec![]
            } else {
                vec![Code::TimeOfDayNoClock]
            };
            assert_eq!(codes, expected, "{key}");
        }
    }

    /// The core reads a key segment as the SDK does
    /// (`tests/fixtures/key_segments.json`).
    #[test]
    fn the_key_segment_rule_is_the_shared_one() {
        let cases: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/key_segments.json")).unwrap();
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
