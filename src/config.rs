//! The live parameter path: an in-memory last-value store for
//! `home/config/{unit}/{param}`, seeded from manifest defaults and served over
//! the bus by a queryable (see docs/design.md#the-write-path).
//!
//! Only the core puts on `home/config/**`. A GET without payload reads the
//! current value. A GET with payload is a write request. The payload is
//! validated against the manifest's type and constraint. A valid value is
//! stored, put on the key so subscribers see it, and echoed in an ok reply. An
//! invalid one gets an error reply and the old value stands.
//!
//! Live edits, the values that differ from their default, are saved to a file
//! after every change and restored when the supervisor starts
//! (docs/design.md#repo-and-live-values).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::manifest::{ParamSpec, ParamType};
use crate::repo::House;
use crate::validate::parse_time;

struct StoredParam {
    param_type: ParamType,
    constraint: Constraint,
    /// The manifest default.
    default: Value,
    value: Value,
}

/// The live edits file: every value that differs from its default, with the
/// default it was set under.
#[derive(Serialize, Deserialize)]
struct SavedEdits {
    schema: u32,
    edits: Vec<SavedEdit>,
}

#[derive(Serialize, Deserialize)]
struct SavedEdit {
    unit: String,
    param: String,
    value: Value,
    default: Value,
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
    /// plan-time validation, so anything unusable here is not enforced.
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

/// The live parameter values by unit and name, each with the type and
/// constraint its manifest declares.
pub struct ConfigStore {
    params: Mutex<BTreeMap<(String, String), StoredParam>>,
    /// Serializes each store mutation with its bus put. Without it, a config
    /// write landing between apply's store swap and apply's puts would be
    /// overwritten on the bus by the older repo value, while the store kept
    /// the newer one. Subscribers and readers would disagree until the next
    /// write.
    write_order: tokio::sync::Mutex<()>,
    /// The live edits file. `None` keeps edits in memory only.
    saved: Option<PathBuf>,
}

impl ConfigStore {
    /// A store holding every parameter's manifest default, with the live
    /// edits saved at `saved` restored over them. A saved edit is restored
    /// when its default is still the manifest default and the value meets the
    /// constraint, the rule an apply uses ([`keeps_live`]). Returns the store
    /// and one line per edit restored or dropped, for the supervisor's log.
    pub fn from_house(house: &House, saved: Option<PathBuf>) -> (ConfigStore, Vec<String>) {
        let mut params = build(house);
        let mut notes = Vec::new();
        let mut saved = saved;
        if let Some(path) = &saved {
            if !restore(&mut params, path, &mut notes) {
                notes.push("live edits are kept in memory only".to_string());
                saved = None;
            }
        }
        let store = ConfigStore {
            params: Mutex::new(params),
            write_order: tokio::sync::Mutex::new(()),
            saved,
        };
        // Saving now drops the edits that were not restored from the file.
        store.save(&store.params.lock().expect("config store lock"));
        (store, notes)
    }

    /// Held across a store mutation and the bus puts that announce it.
    pub async fn write_lock(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.write_order.lock().await
    }

    /// Rebuilds the store from a new house. The params in `replaced` are set
    /// to their repo default, as are new params. Every other param keeps its
    /// live value while it meets the new type and constraint. The plan's diff
    /// names `replaced` ([`keeps_live`]), so plan and apply decide alike. A
    /// write that lands after the diff was taken is kept the same way.
    /// Returns the params whose effective value changed, so the caller can
    /// put them on the bus for live subscribers. Params of new units need no
    /// put, because they read their values with a get at startup.
    pub fn replace_from_house(
        &self,
        house: &House,
        replaced: &BTreeSet<(String, String)>,
    ) -> Vec<(String, String, Value)> {
        let mut params = self.params.lock().expect("config store lock");
        let mut fresh = build(house);
        let mut changed = Vec::new();
        for (key, new) in &mut fresh {
            let Some(old) = params.get(key) else {
                continue;
            };
            if !replaced.contains(key) && check(new.param_type, &new.constraint, &old.value).is_ok()
            {
                new.value = old.value.clone();
            }
            if old.value != new.value {
                changed.push((key.0.clone(), key.1.clone(), new.value.clone()));
            }
        }
        *params = fresh;
        self.save(&params);
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

    /// Validates and stores a write. On success it returns the new value,
    /// which the caller puts on the bus. On rejection the old value stands.
    pub fn write(&self, unit: &str, param: &str, value: Value) -> Result<Value, String> {
        let mut params = self.params.lock().expect("config store lock");
        let Some(stored) = params.get_mut(&(unit.to_string(), param.to_string())) else {
            return Err(format!("unknown parameter {unit}/{param}"));
        };
        check(stored.param_type, &stored.constraint, &value)?;
        let value = canonical(stored.param_type, value);
        stored.value = value.clone();
        self.save(&params);
        Ok(value)
    }

    /// Writes every live edit to the saved file, or removes the file when
    /// there are none. The new file is written and synced under a temporary
    /// name, then renamed over the old one, so a crash or a power cut leaves
    /// one of the two whole. A failure is logged and the store keeps working
    /// in memory.
    fn save(&self, params: &BTreeMap<(String, String), StoredParam>) {
        let Some(path) = &self.saved else {
            return;
        };
        let edits: Vec<SavedEdit> = params
            .iter()
            .filter(|(_, p)| p.value != p.default)
            .map(|((unit, param), p)| SavedEdit {
                unit: unit.clone(),
                param: param.clone(),
                value: p.value.clone(),
                default: p.default.clone(),
            })
            .collect();
        let result = if edits.is_empty() {
            match fs::remove_file(path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
                _ => Ok(()),
            }
        } else {
            let text = serde_json::to_string_pretty(&SavedEdits { schema: 1, edits })
                .expect("saved edits serialize");
            // The process id keeps two supervisors on one house from sharing
            // a temporary file.
            let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
            path.parent()
                .map_or(Ok(()), fs::create_dir_all)
                .and_then(|()| {
                    let mut file = fs::File::create(&tmp)?;
                    file.write_all((text + "\n").as_bytes())?;
                    file.sync_all()
                })
                .and_then(|()| fs::rename(&tmp, path))
                .map_err(|e| e.to_string())
        };
        if let Err(err) = result {
            println!(
                "[homeostat] cannot save live edits to {}: {err}",
                path.display()
            );
        }
    }
}

/// Restores the edits saved at `path` over the defaults in `params`. A missing
/// file means there are none. Returns false when the file exists but cannot
/// be read, so the caller stops saving rather than overwrite it. A file that
/// reads but is not a schema 1 live edits file is moved aside to
/// `<path>.bad`, where the owner can still recover it.
fn restore(
    params: &mut BTreeMap<(String, String), StoredParam>,
    path: &Path,
    notes: &mut Vec<String>,
) -> bool {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return true,
        Err(e) => {
            notes.push(format!("cannot read {}: {e}", path.display()));
            return false;
        }
    };
    let saved = match serde_json::from_str::<SavedEdits>(&text) {
        Ok(saved) if saved.schema == 1 => saved,
        unusable => {
            let reason = match unusable {
                Ok(saved) => format!("schema {} is not 1", saved.schema),
                Err(e) => e.to_string(),
            };
            let mut bad = path.as_os_str().to_owned();
            bad.push(".bad");
            let bad = PathBuf::from(bad);
            if let Err(e) = fs::rename(path, &bad) {
                notes.push(format!(
                    "{} is not a live edits file ({reason}) and cannot be moved aside: {e}",
                    path.display()
                ));
                return false;
            }
            notes.push(format!(
                "{} is not a live edits file ({reason}); moved it to {}",
                path.display(),
                bad.display()
            ));
            return true;
        }
    };
    for edit in saved.edits {
        let name = format!("{}/{}", edit.unit, edit.param);
        let Some(stored) = params.get_mut(&(edit.unit, edit.param)) else {
            notes.push(format!("dropped saved {name}: no such parameter"));
            continue;
        };
        if edit.default != stored.default {
            notes.push(format!("dropped saved {name}: its default changed"));
        } else if let Err(reason) = check(stored.param_type, &stored.constraint, &edit.value) {
            notes.push(format!("dropped saved {name}: {reason}"));
        } else {
            let value = canonical(stored.param_type, edit.value);
            notes.push(format!("restored {name} = {value}"));
            stored.value = value;
        }
    }
    true
}

/// A checked value as the store keeps it. An integer for a float param
/// becomes the float it means, as `default_value` does for the repo default.
/// Otherwise `serde_json`'s 5 != 5.0 would plan as permanent drift.
fn canonical(param_type: ParamType, value: Value) -> Value {
    match param_type {
        ParamType::Float => Value::from(value.as_f64().expect("checked as float")),
        _ => value,
    }
}

fn build(house: &House) -> BTreeMap<(String, String), StoredParam> {
    let mut params = BTreeMap::new();
    for unit in &house.units {
        let Some(specs) = &unit.manifest.params else {
            continue;
        };
        for (name, spec) in specs {
            params.insert(
                (unit.manifest.unit.name.clone(), name.clone()),
                StoredParam {
                    param_type: spec.param_type,
                    constraint: Constraint::from_spec(spec),
                    default: default_value(spec),
                    value: default_value(spec),
                },
            );
        }
    }
    params
}

/// Whether an apply keeps a live value. The owner decides through the
/// default and the family through the live value. An apply that leaves the
/// default alone keeps the family's value. An apply that changes the default,
/// or a constraint the live value no longer meets, sets the new default.
/// `old_default` is the default as of the last apply.
pub fn keeps_live(old_default: &Value, spec: &ParamSpec, live: &Value) -> bool {
    *old_default == default_value(spec)
        && check(spec.param_type, &Constraint::from_spec(spec), live).is_ok()
}

/// Whether the spec's default satisfies its own constraint. This is the
/// plan-time check behind the repo-edit parameter path: a default outside the
/// constraint must fail validation before it reaches a running unit (see
/// docs/design.md#repo-and-live-values). A malformed constraint enforces
/// nothing here; plan-time validation reports it separately.
pub fn default_within_constraint(spec: &ParamSpec) -> Result<(), String> {
    check(
        spec.param_type,
        &Constraint::from_spec(spec),
        &default_value(spec),
    )
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

fn check(param_type: ParamType, constraint: &Constraint, value: &Value) -> Result<(), String> {
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
            default: json!("22:00"),
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

    #[test]
    fn integer_write_to_float_param_stores_the_float() {
        let store = ConfigStore {
            params: Mutex::new(BTreeMap::from([(
                ("heating".to_string(), "setpoint".to_string()),
                StoredParam {
                    param_type: ParamType::Float,
                    constraint: Constraint::default(),
                    default: json!(20.0),
                    value: json!(20.0),
                },
            )])),
            write_order: tokio::sync::Mutex::new(()),
            saved: None,
        };
        let stored = store
            .write("heating", "setpoint", json!(21))
            .expect("in-range write");
        assert!(
            stored.is_f64(),
            "stored as a float, not an integer: {stored}"
        );
        assert_eq!(store.read(|_, _| true)[0].2, json!(21.0));
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

    fn level_store(saved: PathBuf, default: i64, max: i64) -> (ConfigStore, Vec<String>) {
        let mut params = BTreeMap::from([(
            ("probe".to_string(), "level".to_string()),
            StoredParam {
                param_type: ParamType::Int,
                constraint: Constraint {
                    min: Some(0.0),
                    max: Some(max as f64),
                    ..Constraint::default()
                },
                default: json!(default),
                value: json!(default),
            },
        )]);
        let mut notes = Vec::new();
        restore(&mut params, &saved, &mut notes);
        let store = ConfigStore {
            params: Mutex::new(params),
            write_order: tokio::sync::Mutex::new(()),
            saved: Some(saved),
        };
        (store, notes)
    }

    fn saved_path(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("homeostat-live-edits-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir.join("data/live-edits.json")
    }

    #[test]
    fn a_saved_edit_is_restored_under_the_same_default() {
        let path = saved_path("same");
        let (store, _) = level_store(path.clone(), 1, 10);
        store.write("probe", "level", json!(7)).expect("in range");
        let (restored, notes) = level_store(path.clone(), 1, 10);
        assert_eq!(restored.read(|_, _| true)[0].2, json!(7));
        assert_eq!(notes, vec!["restored probe/level = 7".to_string()]);
        let _ = fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn a_saved_edit_is_dropped_when_the_default_changed_or_the_constraint_rules_it_out() {
        let path = saved_path("dropped");
        let (store, _) = level_store(path.clone(), 1, 10);
        store.write("probe", "level", json!(7)).expect("in range");
        let (restored, notes) = level_store(path.clone(), 3, 10);
        assert_eq!(restored.read(|_, _| true)[0].2, json!(3));
        assert_eq!(
            notes,
            vec!["dropped saved probe/level: its default changed".to_string()]
        );
        let (restored, notes) = level_store(path.clone(), 1, 5);
        assert_eq!(restored.read(|_, _| true)[0].2, json!(1));
        assert_eq!(
            notes,
            vec!["dropped saved probe/level: 7 is above max 5".to_string()]
        );
        let _ = fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn the_file_holds_only_live_edits() {
        let path = saved_path("only");
        let (store, _) = level_store(path.clone(), 1, 10);
        store.write("probe", "level", json!(7)).expect("in range");
        assert!(path.exists(), "an edit is saved");
        store.write("probe", "level", json!(1)).expect("in range");
        assert!(!path.exists(), "no edits, no file");
        let _ = fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn a_file_that_is_not_live_edits_is_moved_aside() {
        let path = saved_path("garbage");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "not json").unwrap();
        let (restored, notes) = level_store(path.clone(), 1, 10);
        assert_eq!(restored.read(|_, _| true)[0].2, json!(1));
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("is not a live edits file"), "{notes:?}");
        assert!(!path.exists(), "moved aside");
        assert_eq!(
            fs::read_to_string(path.with_extension("json.bad")).unwrap(),
            "not json",
            "the owner can still recover it"
        );
        let _ = fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn a_file_of_another_schema_is_moved_aside() {
        let path = saved_path("schema");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, r#"{"schema": 2, "edits": []}"#).unwrap();
        let (_, notes) = level_store(path.clone(), 1, 10);
        assert!(notes[0].contains("schema 2 is not 1"), "{notes:?}");
        assert!(path.with_extension("json.bad").exists());
        let _ = fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn an_integer_restored_to_a_float_param_is_stored_as_a_float() {
        let mut params = BTreeMap::from([(
            ("heating".to_string(), "setpoint".to_string()),
            StoredParam {
                param_type: ParamType::Float,
                constraint: Constraint::default(),
                default: json!(20.0),
                value: json!(20.0),
            },
        )]);
        let path = saved_path("float");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{"schema": 1, "edits": [{"unit": "heating", "param": "setpoint", "value": 21, "default": 20.0}]}"#,
        )
        .unwrap();
        let mut notes = Vec::new();
        assert!(restore(&mut params, &path, &mut notes));
        let value = &params.values().next().unwrap().value;
        assert!(value.is_f64(), "{value}");
        assert_eq!(*value, json!(21.0));
        let _ = fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn write_rejection_keeps_old_value() {
        let store = ConfigStore {
            params: Mutex::new(BTreeMap::from([(
                ("evening_lights".to_string(), "off_time".to_string()),
                time_spec("20:00", "02:00"),
            )])),
            write_order: tokio::sync::Mutex::new(()),
            saved: None,
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
        assert_eq!(
            read,
            vec![(
                "evening_lights".to_string(),
                "off_time".to_string(),
                json!("23:30"),
            )]
        );
    }
}
