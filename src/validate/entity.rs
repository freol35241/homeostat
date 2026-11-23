//! Per-entity semantics: ownership and binding, family policy, gesture
//! causes and actions, persons and aspect intervals.

use std::collections::BTreeMap;

use crate::error::{Code, Diagnostics, Scoped};
use crate::expand::{self, ExpandedKey};
use crate::keyspace::{is_reserved_word, PSEUDO_ROOMS};
use crate::manifest::{self, FamilyHold, FamilyUntil, UnitKind, ValueType, WriteMode};
use crate::params::parse_time;
use crate::repo::{House, LoadedEntity};

/// `clock` says whether some unit publishes the house clock's minute.
pub(super) fn check_entities(house: &House, clock: bool, diags: &mut Diagnostics) {
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
        if house.kind_of(&entity.owner) == Some(UnitKind::Adapter)
            && entity.file.entity.id.is_none()
        {
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
            Some(unit) if !unit.manifest.unit.kind.binds_entities() => {
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
pub(super) fn has_clock(expanded: &[ExpandedKey]) -> bool {
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
/// `source-unknown-entity`, which `wiring::resolve_sources` reports.
pub(super) fn check_persons(house: &House, diags: &mut Diagnostics) {
    for entity in &house.entities {
        if entity.file.entity.capability != "person" {
            continue;
        }
        let mut diags = diags.in_file(&entity.path);
        if house.kind_of(&entity.owner) == Some(UnitKind::Adapter) {
            diags.error(
                Code::PersonBoundByAdapter,
                &entity.name,
                format!(
                    "adapter \"{}\" binds a person; bind the device as `tracker` and list it in a person file owned by an automation",
                    entity.owner
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
pub(super) fn check_aspect_intervals(house: &House, diags: &mut Diagnostics) {
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

#[cfg(test)]
mod tests {
    use crate::error::{Code, Diagnostics};
    use crate::expand::expand;
    use crate::repo::House;
    use crate::validate::validate;

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
}
