//! The live parameter path: an in-memory last-value store for
//! `home/config/{unit}/{param}`, seeded from manifest defaults and served
//! over the bus by a queryable (see docs/design.md, step 4).
//!
//! Only the core ever puts on `home/config/**`. A GET without payload reads
//! the current value; a GET with payload is a write request: the payload is
//! validated against the manifest's type and constraint, then either stored
//! and put on the key (subscribers see it live) and echoed in an ok reply,
//! or refused with an error reply — the old value stands.

use std::collections::BTreeMap;
use std::sync::Mutex;

use serde_json::Value;

use crate::manifest::{ParamSpec, ParamType};
use crate::repo::House;
use crate::validate::parse_time;

struct StoredParam {
    param_type: ParamType,
    constraint: Constraint,
    value: Value,
}

#[derive(Default)]
struct Constraint {
    min: Option<f64>,
    max: Option<f64>,
    /// (hour, minute) bounds; together they form a window that may span
    /// midnight (`after 20:00, before 02:00`).
    after: Option<(u8, u8)>,
    before: Option<(u8, u8)>,
    allowed: Option<Vec<String>>,
}

impl Constraint {
    /// Builds from a manifest constraint table. The table already passed
    /// plan-time validation; anything unusable is simply not enforced.
    fn from_spec(spec: &ParamSpec) -> Constraint {
        let Some(table) = &spec.constraint else {
            return Constraint::default();
        };
        let num = |key: &str| match table.get(key) {
            Some(toml::Value::Integer(i)) => Some(*i as f64),
            Some(toml::Value::Float(f)) => Some(*f),
            _ => None,
        };
        let time = |key: &str| match table.get(key) {
            Some(toml::Value::String(s)) => parse_time(s),
            _ => None,
        };
        Constraint {
            min: num("min"),
            max: num("max"),
            after: time("after"),
            before: time("before"),
            allowed: match table.get("enum") {
                Some(toml::Value::Array(items)) => Some(
                    items
                        .iter()
                        .filter_map(|i| i.as_str().map(str::to_string))
                        .collect(),
                ),
                _ => None,
            },
        }
    }
}

pub struct ConfigStore {
    params: Mutex<BTreeMap<(String, String), StoredParam>>,
}

impl ConfigStore {
    pub fn from_house(house: &House) -> ConfigStore {
        ConfigStore { params: Mutex::new(build(house)) }
    }

    /// Rebuilds the store from a new house — on apply, every parameter is
    /// set to its repo default (the repo is the system of record; live
    /// drift resets, and the plan showed it). Returns the params whose
    /// effective value changed so the caller can put them on the bus for
    /// live subscribers; params of new units need no put (they seed via
    /// get at startup).
    pub fn replace_from_house(&self, house: &House) -> Vec<(String, String, Value)> {
        let mut params = self.params.lock().expect("config store lock");
        let fresh = build(house);
        let changed = fresh
            .iter()
            .filter(|(key, new)| {
                params.get(*key).is_some_and(|old| old.value != new.value)
            })
            .map(|((unit, param), p)| (unit.clone(), param.clone(), p.value.clone()))
            .collect();
        *params = fresh;
        changed
    }

    /// Current values matching a `(unit, param)` filter, as
    /// `(unit, param, value)` triples.
    pub fn read<F>(&self, matches: F) -> Vec<(String, String, Value)>
    where
        F: Fn(&str, &str) -> bool,
    {
        self.params
            .lock()
            .expect("config store lock")
            .iter()
            .filter(|((unit, param), _)| matches(unit, param))
            .map(|((unit, param), p)| (unit.clone(), param.clone(), p.value.clone()))
            .collect()
    }

    /// Validates and stores a write. On success the new value is returned
    /// (the caller puts it on the bus); on rejection the old value stands.
    pub fn write(&self, unit: &str, param: &str, value: Value) -> Result<Value, String> {
        let mut params = self.params.lock().expect("config store lock");
        let Some(stored) = params.get_mut(&(unit.to_string(), param.to_string())) else {
            return Err(format!("unknown parameter {unit}/{param}"));
        };
        check(stored.param_type, &stored.constraint, &value)?;
        stored.value = value.clone();
        Ok(value)
    }
}

fn build(house: &House) -> BTreeMap<(String, String), StoredParam> {
    let mut params = BTreeMap::new();
    for unit in &house.units {
        let Some(specs) = &unit.manifest.params else { continue };
        for (name, spec) in specs {
            params.insert(
                (unit.manifest.unit.name.clone(), name.clone()),
                StoredParam {
                    param_type: spec.param_type,
                    constraint: Constraint::from_spec(spec),
                    value: default_value(spec),
                },
            );
        }
    }
    params
}

/// Whether the spec's default satisfies its own constraint — the plan-time
/// check behind the repo-edit parameter path: a proposed default outside
/// the constraint must fail validation, never reach a running unit (see
/// docs/design.md, step 6). A malformed constraint enforces nothing here;
/// plan-time validation reports it separately.
pub fn default_within_constraint(spec: &ParamSpec) -> Result<(), String> {
    check(
        spec.param_type,
        &Constraint::from_spec(spec),
        &default_value(spec),
    )
}

pub fn default_value(spec: &ParamSpec) -> Value {
    match &spec.default {
        toml::Value::Boolean(b) => Value::from(*b),
        toml::Value::Integer(i) => Value::from(*i),
        toml::Value::Float(f) => Value::from(*f),
        toml::Value::String(s) => Value::from(s.clone()),
        other => Value::from(other.to_string()),
    }
}

fn check(param_type: ParamType, constraint: &Constraint, value: &Value) -> Result<(), String> {
    match param_type {
        ParamType::Bool => {
            value.as_bool().ok_or_else(|| type_error(param_type, value))?;
        }
        ParamType::Int => {
            let n = value.as_i64().ok_or_else(|| type_error(param_type, value))?;
            check_range(n as f64, constraint)?;
        }
        ParamType::Float => {
            let n = value.as_f64().ok_or_else(|| type_error(param_type, value))?;
            check_range(n, constraint)?;
        }
        ParamType::String => {
            let s = value.as_str().ok_or_else(|| type_error(param_type, value))?;
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
            let s = value.as_str().ok_or_else(|| type_error(param_type, value))?;
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
    let minutes = |(h, m): (u8, u8)| h as u16 * 60 + m as u16;
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn time_spec(after: &str, before: &str) -> StoredParam {
        StoredParam {
            param_type: ParamType::Time,
            constraint: Constraint {
                after: parse_time(after),
                before: parse_time(before),
                ..Constraint::default()
            },
            value: json!("22:00"),
        }
    }

    fn check_param(p: &StoredParam, value: Value) -> Result<(), String> {
        check(p.param_type, &p.constraint, &value)
    }

    #[test]
    fn time_window_spans_midnight() {
        let p = time_spec("20:00", "02:00");
        for ok in ["20:00", "23:59", "00:30", "02:00"] {
            check_param(&p, json!(ok)).unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
        for bad in ["19:59", "02:01", "12:00"] {
            assert!(check_param(&p, json!(bad)).is_err(), "{bad} accepted");
        }
    }

    #[test]
    fn time_window_same_day() {
        let p = time_spec("08:00", "17:00");
        check_param(&p, json!("12:00")).unwrap();
        assert!(check_param(&p, json!("18:00")).is_err());
        assert!(check_param(&p, json!("07:59")).is_err());
    }

    #[test]
    fn time_rejects_non_times() {
        let p = time_spec("20:00", "02:00");
        for bad in [json!("not-a-time"), json!("25:00"), json!(22), json!(true)] {
            assert!(check_param(&p, bad.clone()).is_err(), "{bad} accepted");
        }
    }

    #[test]
    fn int_range() {
        let c = Constraint { min: Some(0.0), max: Some(100.0), ..Constraint::default() };
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

    #[test]
    fn write_rejection_keeps_old_value() {
        let store = ConfigStore {
            params: Mutex::new(BTreeMap::from([(
                ("evening_lights".to_string(), "off_time".to_string()),
                time_spec("20:00", "02:00"),
            )])),
        };
        store
            .write("evening_lights", "off_time", json!("23:30"))
            .expect("in-window write accepted");
        store
            .write("evening_lights", "off_time", json!("03:00"))
            .expect_err("out-of-window write rejected");
        store
            .write("evening_lights", "nope", json!("23:00"))
            .expect_err("unknown parameter rejected");
        let read = store.read(|_, _| true);
        assert_eq!(read, vec![(
            "evening_lights".to_string(),
            "off_time".to_string(),
            json!("23:30"),
        )]);
    }
}
