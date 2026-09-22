use std::collections::BTreeMap;

use crate::error::ValidationError;
use crate::keyspace::{is_reserved_word, PSEUDO_ROOMS};
use crate::manifest::{
    DiscoveryMode, ParamSpec, ParamType, UnitKind, WidgetKind, WidgetSpec, WriteMode, CAPABILITIES,
    VOCABULARY,
};
use crate::repo::House;

/// Structural validation that does not involve key expansion or grants:
/// uniqueness, capabilities, reserved rooms, ownership, zones, params.
pub fn validate(house: &House) -> Vec<ValidationError> {
    let mut errors = Vec::new();

    check_names(house, &mut errors);
    check_duplicates(house, &mut errors);
    check_manifest_shape(house, &mut errors);
    check_entities(house, &mut errors);
    check_zones(house, &mut errors);
    check_params(house, &mut errors);
    check_dashboard(house, &mut errors);

    errors
}

/// Whether a name is usable as exactly one bus key segment. Anything else
/// either breaks the fixed key schema (`/`), is meaningful to the bus
/// (`*`, `$`, `?`, `#`), or invites whitespace/encoding surprises — and a
/// bad unit name would panic the supervisor's liveliness subscriber.
fn valid_segment(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

/// Every name that becomes a key segment: unit and parameter names
/// (`home/config/{unit}/{param}`, `home/meta/{unit}/...`), entity and room
/// names (`home/state/{room}/{entity}/{aspect}`), zone names (expanded in
/// key expressions). The unit name `system` is refused outright: the core
/// serves `home/meta/system/**` itself.
fn check_names(house: &House, errors: &mut Vec<ValidationError>) {
    let segment_message = |what: &str, name: &str| {
        format!(
            "{what} \"{name}\" must be a single key segment (letters, digits, \"_\", \"-\", \".\")"
        )
    };
    for unit in &house.units {
        let name = &unit.manifest.unit.name;
        let file = Some(unit.path.clone());
        if !valid_segment(name) {
            errors.push(ValidationError::new(
                "invalid-name",
                name,
                segment_message("unit name", name),
                file.clone(),
            ));
        } else if name == "system" {
            errors.push(ValidationError::new(
                "reserved-unit-name",
                name,
                "unit name \"system\" is reserved for the core's meta keys",
                file.clone(),
            ));
        }
        if let Some(params) = &unit.manifest.params {
            for param in params.keys() {
                if !valid_segment(param) {
                    errors.push(ValidationError::new(
                        "invalid-name",
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
                "invalid-name",
                &entity.name,
                segment_message("entity name", &entity.name),
                file.clone(),
            ));
        }
        let room = &entity.file.entity.room;
        if !valid_segment(room) {
            errors.push(ValidationError::new(
                "invalid-name",
                &entity.name,
                segment_message("room", room),
                file,
            ));
        }
    }
    for zone in house.zones.keys() {
        if !valid_segment(zone) {
            errors.push(ValidationError::new(
                "invalid-name",
                zone,
                segment_message("zone name", zone),
                Some("zones.toml".to_string()),
            ));
        }
    }
    for view in house.dashboard.iter().flat_map(|d| &d.view) {
        if !valid_segment(&view.name) {
            errors.push(ValidationError::new(
                "invalid-name",
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
        entity_paths
            .entry(&entity.name)
            .or_default()
            .push(&entity.path);
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

    // The binding `id` is the device address within one owner's namespace;
    // two files sharing it would silently collapse to whichever the
    // adapter's `by_id` map keeps last.
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
            paths.sort();
            errors.push(ValidationError::new(
                "duplicate-entity-id",
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
                    "invalid-manifest",
                    name,
                    message,
                    file.clone(),
                ));
            }
        }
        if unit.manifest.unit.kind == UnitKind::Adapter {
            if unit.manifest.entities.is_none() {
                errors.push(ValidationError::new(
                    "invalid-manifest",
                    name,
                    "adapter requires an [entities] section",
                    file.clone(),
                ));
            }
            if unit.manifest.discovery.is_none() {
                errors.push(ValidationError::new(
                    "invalid-manifest",
                    name,
                    "adapter requires a [discovery] section",
                    file.clone(),
                ));
            }
        } else {
            // Automations may bind entities (virtual sensors); services
            // have never needed to and stay refused until one does.
            if unit.manifest.entities.is_some() && unit.manifest.unit.kind != UnitKind::Automation {
                errors.push(ValidationError::new(
                    "invalid-manifest",
                    name,
                    "[entities] is only valid for adapters and automations",
                    file.clone(),
                ));
            }
            // Services may talk to an external backend (e.g. the recorder's
            // store) and use [discovery] the same way adapters do.
            if unit.manifest.discovery.is_some() && unit.manifest.unit.kind != UnitKind::Service {
                errors.push(ValidationError::new(
                    "invalid-manifest",
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
                "unknown-capability",
                &entity.name,
                format!("unknown capability \"{capability}\""),
                file.clone(),
            ));
        }

        // A write mode governs commands, so it means nothing on a
        // capability that takes none — eight of the fourteen. It stays
        // required where there IS something to govern, so a light or a
        // lock never inherits a policy silently.
        if entity.file.write_policy.mode.is_none()
            && VOCABULARY
                .iter()
                .any(|c| c.name == capability && c.base.is_some())
        {
            errors.push(ValidationError::new(
                "write-mode-required",
                &entity.name,
                format!(
                    "capability \"{capability}\" takes commands, so [write_policy] needs a mode"
                ),
                file.clone(),
            ));
        }

        // An adapter binds periphery, so its entity files address it. An
        // automation's do not: a computed value has no device behind it,
        // and requiring an `id` there only made units invent one.
        let owner_is_adapter = house
            .unit(&entity.file.write_policy.owner)
            .is_some_and(|u| u.manifest.unit.kind == UnitKind::Adapter);
        if owner_is_adapter && entity.file.entity.id.is_none() {
            errors.push(ValidationError::new(
                "entity-id-required",
                &entity.name,
                "an adapter-owned entity needs an [entity] id: its adapter-native address",
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
                "missing-owner-unit",
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
                    "missing-owner-unit",
                    &entity.name,
                    format!("owner \"{owner}\" is not an adapter or automation"),
                    file.clone(),
                ));
            }
            Some(_) if owner != &entity.owner => {
                errors.push(ValidationError::new(
                    "owner-mismatch",
                    &entity.name,
                    format!("owner \"{owner}\" but bound by unit \"{}\"", entity.owner),
                    file.clone(),
                ));
            }
            Some(unit)
                if unit.manifest.unit.kind == UnitKind::Automation
                    && entity.file.write_policy.mode() == WriteMode::Arbitrated =>
            {
                // A commandable virtual entity is a latch (docs/design.md,
                // Commandable virtual entities): no device to contend for,
                // no hold to expire, so arbitration has nothing to order.
                errors.push(ValidationError::new(
                    "virtual-entity-arbitrated",
                    &entity.name,
                    "automation-owned entities are latches and cannot be arbitrated; use shared or exclusive",
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
            if PSEUDO_ROOMS.contains(&room.as_str()) {
                errors.push(ValidationError::new(
                    "zone-pseudo-room",
                    zone,
                    format!("zone includes pseudo-room \"{room}\""),
                    file.clone(),
                ));
            } else if !rooms.contains(&room.as_str()) {
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

    // The default must satisfy its own constraint: the repo-edit parameter
    // path is enforced here, so an out-of-constraint default
    // never plans, let alone reaches a running unit. Skipped when this
    // param already has errors — a default judged against a malformed
    // constraint would only add noise.
    // errors.len() == before already implies the default matched its type.
    if errors.len() == before {
        if let Err(message) = crate::config::default_within_constraint(spec) {
            errors.push(ValidationError::new(
                "invalid-default",
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

pub(crate) fn parse_time(s: &str) -> Option<(u8, u8)> {
    let (hh, mm) = s.split_once(':')?;
    let hh: u8 = hh.parse().ok()?;
    let mm: u8 = mm.parse().ok()?;
    (hh < 24 && mm < 60).then_some((hh, mm))
}

/// `dashboard.toml`: each view is a generated one or a composition, never
/// both; each widget carries exactly the fields its kind takes, and every
/// reference resolves against the house. The `[dashboard]` table on an
/// entity file is retired in favour of a `tile` widget here.
fn check_dashboard(house: &House, errors: &mut Vec<ValidationError>) {
    for entity in &house.entities {
        if entity.file.dashboard.is_some() {
            errors.push(ValidationError::new(
                "entity-dashboard-retired",
                &entity.name,
                format!(
                    "[dashboard] on an entity is retired: place it with \
                     {{ kind = \"tile\", entity = \"{}\" }} on a view in dashboard.toml",
                    entity.name
                ),
                Some(entity.path.clone()),
            ));
        }
    }
    let Some(dashboard) = &house.dashboard else {
        return;
    };
    let file = Some("dashboard.toml".to_string());
    let rooms = house.rooms();
    let mut seen: Vec<&str> = Vec::new();
    for view in &dashboard.view {
        if matches!(view.name.as_str(), "health" | "notshown") {
            errors.push(ValidationError::new(
                "dashboard-reserved-view",
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
                "dashboard-duplicate-view",
                &view.name,
                format!("view \"{}\" is declared more than once", view.name),
                file.clone(),
            ));
        }
        seen.push(&view.name);
        if view.kind.is_some() == !view.widgets.is_empty() {
            errors.push(ValidationError::new(
                "dashboard-view-shape",
                &view.name,
                "a view is a generated `kind` or a list of `widgets`, never both or neither",
                file.clone(),
            ));
        }
        for (i, widget) in view.widgets.iter().enumerate() {
            let subject = format!("{}[{i}]", view.name);
            check_widget(house, &rooms, widget, &subject, &file, errors);
            // A group's members are widgets like any other, and take the
            // same checks. One level only: a group of groups is a layout
            // language, which the file is deliberately not.
            for (j, member) in widget.widgets.iter().enumerate() {
                let subject = format!("{subject}[{j}]");
                if member.kind == WidgetKind::Group {
                    errors.push(ValidationError::new(
                        "dashboard-nested-group",
                        &subject,
                        "a group holds widgets, never another group",
                        file.clone(),
                    ));
                    continue;
                }
                check_widget(house, &rooms, member, &subject, &file, errors);
            }
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
    file: &Option<String>,
    errors: &mut Vec<ValidationError>,
) {
    if let Some(message) = widget_fields_message(widget) {
        errors.push(ValidationError::new(
            "dashboard-widget-fields",
            subject,
            message,
            file.clone(),
        ));
        return;
    }
    if let Some(entity) = &widget.entity {
        match house.entities.iter().find(|e| &e.name == entity) {
            None => errors.push(ValidationError::new(
                "dashboard-unknown-entity",
                subject,
                format!("widget names unknown entity \"{entity}\""),
                file.clone(),
            )),
            // A capability widget draws that capability's vocabulary, so
            // it is only meaningful over an entity that speaks it.
            Some(e)
                if widget.kind == WidgetKind::Burner && e.file.entity.capability != "burner" =>
            {
                errors.push(ValidationError::new(
                    "dashboard-widget-capability",
                    subject,
                    format!(
                        "a `burner` widget needs a burner; \"{entity}\" is a {}",
                        e.file.entity.capability
                    ),
                    file.clone(),
                ))
            }
            Some(_) => {}
        }
    }
    if let Some(aspect) = &widget.aspect {
        if !valid_segment(aspect) {
            errors.push(ValidationError::new(
                "dashboard-invalid-aspect",
                subject,
                format!("aspect \"{aspect}\" must be a single key segment"),
                file.clone(),
            ));
        }
    }
    if let Some(room) = &widget.room {
        if !rooms.contains(&room.as_str()) {
            errors.push(ValidationError::new(
                "dashboard-unknown-room",
                subject,
                format!("widget names unknown room \"{room}\""),
                file.clone(),
            ));
        }
    }
    if let Some(unit) = &widget.unit {
        if house.unit(unit).is_none() {
            errors.push(ValidationError::new(
                "dashboard-unknown-unit",
                subject,
                format!("widget names unknown unit \"{unit}\""),
                file.clone(),
            ));
        }
    }
}

/// The fields a widget kind takes — required ones first, then optional —
/// against what it carries; None when they agree.
fn widget_fields_message(widget: &WidgetSpec) -> Option<String> {
    let (required, optional): (&[&str], &[&str]) = match widget.kind {
        WidgetKind::Tile | WidgetKind::Dial => (&["entity"], &["aspect"]),
        WidgetKind::Burner => (&["entity"], &[]),
        WidgetKind::Chart => (&["entity", "aspect"], &["hours"]),
        WidgetKind::Entity => (&["entity"], &[]),
        WidgetKind::Room => (&["room"], &[]),
        WidgetKind::Unit | WidgetKind::Params => (&["unit"], &[]),
        WidgetKind::People | WidgetKind::Deviations | WidgetKind::Map => (&[], &[]),
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
    None
}
