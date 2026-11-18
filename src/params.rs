//! What parameter values mean. A manifest constraint is parsed once here,
//! for validation and for the live store. This module also checks a value
//! against its type and constraint, parses the `"HH:MM"` time of day, and
//! turns a default into the JSON value the bus carries.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::error::Code;
use crate::manifest::{ParamSpec, ParamType};

/// A parameter's constraint as the store enforces it.
#[derive(Default)]
pub(crate) struct Constraint {
    pub(crate) min: Option<f64>,
    pub(crate) max: Option<f64>,
    /// (hour, minute) bounds; together they form a window that may span
    /// midnight (`after 20:00, before 02:00`).
    pub(crate) after: Option<(u8, u8)>,
    pub(crate) before: Option<(u8, u8)>,
    pub(crate) allowed: Option<Vec<String>>,
}

impl Constraint {
    /// Reads a parameter's declaration into the constraint the store
    /// enforces, or every error in the declaration with its code.
    pub(crate) fn parse(spec: &ParamSpec) -> Result<Constraint, Vec<(Code, String)>> {
        let t = spec.param_type;
        let mut constraint = Constraint::default();
        let mut errors = Vec::new();

        let default_ok = match (t, &spec.default) {
            (ParamType::Bool, toml::Value::Boolean(_))
            | (ParamType::Int, toml::Value::Integer(_))
            | (ParamType::Float, toml::Value::Float(_) | toml::Value::Integer(_))
            | (ParamType::String, toml::Value::String(_)) => true,
            (ParamType::Time, toml::Value::String(s)) => parse_time(s).is_some(),
            _ => false,
        };
        if !default_ok {
            errors.push((
                Code::InvalidDefault,
                format!(
                    "default {} does not match type \"{t}\"",
                    display_value(&spec.default)
                ),
            ));
        }

        let no_table = BTreeMap::new();
        let table = spec.constraint.as_ref().unwrap_or(&no_table);
        for (key, value) in table {
            let valid_for_type = match key.as_str() {
                "min" | "max" => matches!(t, ParamType::Int | ParamType::Float),
                "after" | "before" => t == ParamType::Time,
                "enum" => t == ParamType::String,
                _ => {
                    errors.push((
                        Code::MalformedConstraint,
                        format!("unknown constraint \"{key}\""),
                    ));
                    continue;
                }
            };
            if !valid_for_type {
                errors.push((
                    Code::MalformedConstraint,
                    format!("constraint \"{key}\" is not valid for type \"{t}\""),
                ));
                continue;
            }
            let (value_ok, expected) = match key.as_str() {
                "min" => {
                    constraint.min = as_number(value);
                    (constraint.min.is_some(), "a number")
                }
                "max" => {
                    constraint.max = as_number(value);
                    (constraint.max.is_some(), "a number")
                }
                "after" => {
                    constraint.after = value.as_str().and_then(parse_time);
                    (constraint.after.is_some(), "a time (HH:MM)")
                }
                "before" => {
                    constraint.before = value.as_str().and_then(parse_time);
                    (constraint.before.is_some(), "a time (HH:MM)")
                }
                _ => {
                    constraint.allowed = value
                        .as_array()
                        .filter(|items| !items.is_empty())
                        .and_then(|items| {
                            items
                                .iter()
                                .map(|i| i.as_str().map(str::to_string))
                                .collect()
                        });
                    (constraint.allowed.is_some(), "a non-empty array of strings")
                }
            };
            if !value_ok {
                errors.push((
                    Code::MalformedConstraint,
                    format!("constraint \"{key}\" must be {expected}"),
                ));
            }
        }

        if let (Some(min), Some(max)) = (
            table.get("min").and_then(as_number),
            table.get("max").and_then(as_number),
        ) {
            if min > max {
                errors.push((
                    Code::MalformedConstraint,
                    format!(
                        "min ({}) is greater than max ({})",
                        display_value(&table["min"]),
                        display_value(&table["max"])
                    ),
                ));
            }
        }

        // The default must satisfy its own constraint, so an out-of-constraint
        // default does not plan (docs/design.md#repo-and-live-values). The
        // check is skipped when the declaration already has errors, because
        // judging a default against a malformed constraint only adds noise.
        if errors.is_empty() {
            if let Err(message) = check(t, &constraint, &default_value(spec)) {
                errors.push((
                    Code::InvalidDefault,
                    format!("default {}: {message}", display_value(&spec.default)),
                ));
            }
        }
        if errors.is_empty() {
            Ok(constraint)
        } else {
            Err(errors)
        }
    }
}

/// Whether an apply keeps a live value. The owner decides through the
/// default and the family through the live value. An apply that leaves the
/// default alone keeps the family's value. An apply that changes the default,
/// or a constraint the live value no longer meets, sets the new default.
/// `old_default` is the default as of the last apply.
pub fn keeps_live(old_default: &Value, spec: &ParamSpec, live: &Value) -> bool {
    *old_default == default_value(spec)
        && check(
            spec.param_type,
            &Constraint::parse(spec).unwrap_or_default(),
            live,
        )
        .is_ok()
}

/// A parameter's manifest default as the JSON value the bus carries.
pub fn default_value(spec: &ParamSpec) -> Value {
    match &spec.default {
        toml::Value::Boolean(b) => Value::from(*b),
        // An integer default on a float param becomes the float it means.
        // serde_json's Number(5) != Number(5.0), so an integral default would
        // make every decimal write of the same value plan as permanent drift.
        toml::Value::Integer(i) if spec.param_type == ParamType::Float => Value::from(*i as f64),
        toml::Value::Integer(i) => Value::from(*i),
        toml::Value::Float(f) => Value::from(*f),
        toml::Value::String(s) => Value::from(s.clone()),
        other => Value::from(other.to_string()),
    }
}

/// Checks a JSON value against a parameter's type and constraint. The error
/// names the violation.
pub(crate) fn check(
    param_type: ParamType,
    constraint: &Constraint,
    value: &Value,
) -> Result<(), String> {
    match param_type {
        ParamType::Bool => {
            value
                .as_bool()
                .ok_or_else(|| type_error(param_type, value))?;
        }
        ParamType::Int => {
            let n = value
                .as_i64()
                .ok_or_else(|| type_error(param_type, value))?;
            check_range(n as f64, constraint)?;
        }
        ParamType::Float => {
            let n = value
                .as_f64()
                .ok_or_else(|| type_error(param_type, value))?;
            check_range(n, constraint)?;
        }
        ParamType::String => {
            let s = value
                .as_str()
                .ok_or_else(|| type_error(param_type, value))?;
            if let Some(allowed) = &constraint.allowed {
                if !allowed.iter().any(|a| a == s) {
                    return Err(format!(
                        "\"{s}\" is not one of {}",
                        allowed
                            .iter()
                            .map(|a| format!("\"{a}\""))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
            }
        }
        ParamType::Time => {
            let s = value
                .as_str()
                .ok_or_else(|| type_error(param_type, value))?;
            let t = parse_time(s).ok_or_else(|| format!("\"{s}\" is not a time (HH:MM)"))?;
            check_window(t, constraint)?;
        }
    }
    Ok(())
}

fn type_error(param_type: ParamType, value: &Value) -> String {
    format!("{value} does not match type \"{param_type}\"")
}

fn check_range(n: f64, constraint: &Constraint) -> Result<(), String> {
    if let Some(min) = constraint.min {
        if n < min {
            return Err(format!("{n} is below min {min}"));
        }
    }
    if let Some(max) = constraint.max {
        if n > max {
            return Err(format!("{n} is above max {max}"));
        }
    }
    Ok(())
}

/// `after`/`before` form an inclusive window that may span midnight:
/// `after 20:00, before 02:00` admits 20:00..=23:59 and 00:00..=02:00.
fn check_window(t: (u8, u8), constraint: &Constraint) -> Result<(), String> {
    let minutes = |(h, m): (u8, u8)| u16::from(h) * 60 + u16::from(m);
    let v = minutes(t);
    let err = |bound: &str, (h, m): (u8, u8)| {
        Err(format!(
            "{:02}:{:02} is not {bound} {h:02}:{m:02}",
            t.0, t.1
        ))
    };
    match (constraint.after, constraint.before) {
        (Some(a), Some(b)) => {
            let inside = if minutes(a) <= minutes(b) {
                v >= minutes(a) && v <= minutes(b)
            } else {
                v >= minutes(a) || v <= minutes(b)
            };
            if !inside {
                return Err(format!(
                    "{:02}:{:02} is outside {:02}:{:02}..{:02}:{:02}",
                    t.0, t.1, a.0, a.1, b.0, b.1
                ));
            }
        }
        (Some(a), None) if v < minutes(a) => return err("after", a),
        (None, Some(b)) if v > minutes(b) => return err("before", b),
        _ => {}
    }
    Ok(())
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
/// each side. It is the form every reader accepts: the SDK reads a `time`
/// parameter with Python's `time.fromisoformat`, which refuses `7:30`, and
/// the page and the arbiter read `HH:MM` only. `tests/fixtures/time_of_day.json`
/// holds the cases every reader is tested against.
pub(crate) fn parse_time(s: &str) -> Option<(u8, u8)> {
    let b = s.as_bytes();
    if b.len() != 5 || b[2] != b':' || ![0, 1, 3, 4].iter().all(|&i| b[i].is_ascii_digit()) {
        return None;
    }
    let hh = (b[0] - b'0') * 10 + (b[1] - b'0');
    let mm = (b[3] - b'0') * 10 + (b[4] - b'0');
    (hh < 24 && mm < 60).then_some((hh, mm))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use serde_json::json;

    fn time_window(after: &str, before: &str) -> Constraint {
        Constraint {
            after: parse_time(after),
            before: parse_time(before),
            ..Constraint::default()
        }
    }

    #[test]
    fn time_window_spans_midnight() {
        let c = time_window("20:00", "02:00");
        for ok in ["20:00", "23:59", "00:30", "02:00"] {
            check(ParamType::Time, &c, &json!(ok)).unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
        for bad in ["19:59", "02:01", "12:00"] {
            assert!(
                check(ParamType::Time, &c, &json!(bad)).is_err(),
                "{bad} accepted"
            );
        }
    }

    #[test]
    fn time_window_same_day() {
        let c = time_window("08:00", "17:00");
        check(ParamType::Time, &c, &json!("12:00")).unwrap();
        assert!(check(ParamType::Time, &c, &json!("18:00")).is_err());
        assert!(check(ParamType::Time, &c, &json!("07:59")).is_err());
    }

    #[test]
    fn time_rejects_non_times() {
        let c = time_window("20:00", "02:00");
        // "7:30" and "+7:30" too: the SDK reads a time with `fromisoformat`.
        for bad in [
            json!("not-a-time"),
            json!("25:00"),
            json!("7:30"),
            json!("+7:30"),
            json!(22),
            json!(true),
        ] {
            assert!(check(ParamType::Time, &c, &bad).is_err(), "{bad} accepted");
        }
    }

    #[test]
    fn int_range() {
        let c = Constraint {
            min: Some(0.0),
            max: Some(100.0),
            ..Constraint::default()
        };
        check(ParamType::Int, &c, &json!(50)).unwrap();
        assert!(check(ParamType::Int, &c, &json!(-1)).is_err());
        assert!(check(ParamType::Int, &c, &json!(101)).is_err());
        assert!(check(ParamType::Int, &c, &json!(1.5)).is_err());
    }

    #[test]
    fn string_enum() {
        let c = Constraint {
            allowed: Some(vec!["low".into(), "high".into()]),
            ..Constraint::default()
        };
        check(ParamType::String, &c, &json!("low")).unwrap();
        assert!(check(ParamType::String, &c, &json!("medium")).is_err());
        assert!(check(ParamType::String, &c, &json!(3)).is_err());
    }

    /// The messages the corpus does not pin, one declaration each.
    #[test]
    fn a_malformed_declaration_reports_every_error() {
        let cases = [
            (
                r#"type = "int"
default = 1
constraint = { step = 2 }"#,
                vec![(Code::MalformedConstraint, r#"unknown constraint "step""#)],
            ),
            (
                r#"type = "int"
default = 1
constraint = { min = "low" }"#,
                vec![(
                    Code::MalformedConstraint,
                    r#"constraint "min" must be a number"#,
                )],
            ),
            (
                r#"type = "time"
default = "08:00"
constraint = { after = "8am" }"#,
                vec![(
                    Code::MalformedConstraint,
                    r#"constraint "after" must be a time (HH:MM)"#,
                )],
            ),
            (
                r#"type = "string"
default = "a"
constraint = { enum = [] }"#,
                vec![(
                    Code::MalformedConstraint,
                    r#"constraint "enum" must be a non-empty array of strings"#,
                )],
            ),
            (
                r#"type = "int"
default = "one"
constraint = { min = 0 }"#,
                vec![(
                    Code::InvalidDefault,
                    r#"default one does not match type "int""#,
                )],
            ),
            (
                r#"type = "string"
default = "a"
constraint = { min = 2, max = 1 }"#,
                vec![
                    (
                        Code::MalformedConstraint,
                        r#"constraint "max" is not valid for type "string""#,
                    ),
                    (
                        Code::MalformedConstraint,
                        r#"constraint "min" is not valid for type "string""#,
                    ),
                    (Code::MalformedConstraint, "min (2) is greater than max (1)"),
                ],
            ),
        ];
        for (declaration, expected) in cases {
            let spec: ParamSpec = toml::from_str(declaration).unwrap();
            let Err(errors) = Constraint::parse(&spec) else {
                panic!("{declaration} parsed");
            };
            let expected: Vec<(Code, String)> = expected
                .into_iter()
                .map(|(c, m)| (c, m.to_string()))
                .collect();
            assert_eq!(errors, expected, "{declaration}");
        }
    }

    #[test]
    fn a_declaration_without_a_constraint_parses() {
        let spec: ParamSpec = toml::from_str("type = \"float\"\ndefault = 1").unwrap();
        assert!(Constraint::parse(&spec).is_ok());
    }

    fn level_spec(default: i64, max: i64) -> ParamSpec {
        ParamSpec {
            param_type: ParamType::Int,
            default: toml::Value::Integer(default),
            constraint: Some(BTreeMap::from([
                ("min".to_string(), toml::Value::Integer(0)),
                ("max".to_string(), toml::Value::Integer(max)),
            ])),
            editable_by: None,
        }
    }

    #[test]
    fn a_live_edit_under_an_unchanged_default_is_kept() {
        assert!(keeps_live(&json!(1), &level_spec(1, 10), &json!(7)));
    }

    #[test]
    fn a_changed_default_replaces_a_live_edit() {
        assert!(!keeps_live(&json!(1), &level_spec(3, 10), &json!(7)));
    }

    #[test]
    fn a_live_edit_outside_the_new_constraint_is_replaced() {
        assert!(!keeps_live(&json!(1), &level_spec(1, 5), &json!(7)));
        assert!(keeps_live(&json!(1), &level_spec(1, 5), &json!(4)));
    }

    #[test]
    fn a_live_edit_of_another_type_is_replaced() {
        assert!(!keeps_live(&json!(1), &level_spec(1, 10), &json!("7")));
    }

    /// The core reads a time of day as every other reader does
    /// (`tests/fixtures/time_of_day.json`).
    #[test]
    fn the_time_of_day_grammar_is_the_shared_one() {
        let cases: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/time_of_day.json")).unwrap();
        for s in cases["valid"].as_array().unwrap() {
            let s = s.as_str().unwrap();
            let (hh, mm) = parse_time(s).unwrap_or_else(|| panic!("{s:?} is a time of day"));
            assert_eq!(format!("{hh:02}:{mm:02}"), s);
        }
        for s in cases["invalid"].as_array().unwrap() {
            let s = s.as_str().unwrap();
            assert!(parse_time(s).is_none(), "{s:?} is not a time of day");
        }
        assert_eq!(parse_time("07:30"), Some((7, 30)));
    }
}
