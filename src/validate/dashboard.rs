//! Checks on `dashboard.toml`: its controls and widgets name entities,
//! aspects and parameters the house has, and each widget carries the fields
//! its kind takes.

use crate::error::{Code, Diagnostics, Scoped};
use crate::manifest::{ControlSpec, WidgetKind, WidgetSpec};
use crate::repo::House;

use super::valid_segment;

/// `dashboard.toml`: each view is either a generated one or a composition.
/// Each widget carries the fields its kind takes and no others, and every
/// reference resolves against the house.
pub(super) fn check_dashboard(house: &House, diags: &mut Diagnostics) {
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
