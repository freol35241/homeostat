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

use crate::manifest::ParamType;
use crate::params::{check, default_value, Constraint};
use crate::repo::House;

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
    /// constraint, the rule an apply uses
    /// ([`keeps_live`](crate::params::keeps_live)). Returns the store
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
    /// names `replaced` ([`keeps_live`](crate::params::keeps_live)), so plan
    /// and apply decide alike. A write that lands after the diff was taken is
    /// kept the same way.
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
                    constraint: Constraint::parse(spec).unwrap_or_default(),
                    default: default_value(spec),
                    value: default_value(spec),
                },
            );
        }
    }
    params
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::parse_time;
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
