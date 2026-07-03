use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::error::ValidationError;
use crate::manifest::{EntityFile, UnitKind, UnitManifest, ZonesFile, SUPPORTED_SCHEMA};

#[derive(Debug)]
pub struct LoadedUnit {
    pub manifest: UnitManifest,
    /// House-relative path, e.g. `units/zigbee.toml`.
    pub path: String,
}

#[derive(Debug)]
pub struct LoadedEntity {
    /// The globally unique entity name: the file stem of the entity file.
    pub name: String,
    pub file: EntityFile,
    /// House-relative path, e.g. `entities/zigbee/kitchen_ceiling.toml`.
    pub path: String,
    /// Name of the adapter unit whose entities dir this file lives in.
    pub adapter: String,
}

#[derive(Debug, Default)]
pub struct House {
    pub units: Vec<LoadedUnit>,
    pub entities: Vec<LoadedEntity>,
    pub zones: BTreeMap<String, Vec<String>>,
}

impl House {
    pub fn unit(&self, name: &str) -> Option<&LoadedUnit> {
        self.units.iter().find(|u| u.manifest.unit.name == name)
    }

    /// All rooms that house at least one entity.
    pub fn rooms(&self) -> Vec<&str> {
        let mut rooms: Vec<&str> = self.entities.iter().map(|e| e.file.entity.room.as_str()).collect();
        rooms.sort();
        rooms.dedup();
        rooms
    }
}

/// Reads a house repo. Parse failures never abort the walk: the result is a
/// typed model of everything that parsed plus a complete error list.
pub fn load(root: &Path) -> (House, Vec<ValidationError>) {
    let mut house = House::default();
    let mut errors = Vec::new();

    let zones_path = root.join("zones.toml");
    if zones_path.exists() {
        if let Some(zones) = read_toml::<ZonesFile>(&zones_path, "zones.toml", &mut errors) {
            check_schema_version(zones.schema, "zones", "zones.toml", &mut errors);
            house.zones = zones.zones;
        }
    }

    let units_dir = root.join("units");
    if !units_dir.is_dir() {
        errors.push(ValidationError::new(
            "missing-units-dir",
            "units",
            "directory not found",
            None,
        ));
        return (house, errors);
    }
    for file in toml_files(&units_dir) {
        let rel = format!("units/{file}");
        let Some(manifest) = read_toml::<UnitManifest>(&units_dir.join(&file), &rel, &mut errors)
        else {
            continue;
        };
        check_schema_version(manifest.schema, &manifest.unit.name, &rel, &mut errors);
        house.units.push(LoadedUnit { manifest, path: rel });
    }

    let mut entity_dirs: Vec<(String, String)> = Vec::new(); // (adapter, dir)
    for unit in &house.units {
        if unit.manifest.unit.kind != UnitKind::Adapter {
            continue;
        }
        if let Some(entities) = &unit.manifest.entities {
            entity_dirs.push((unit.manifest.unit.name.clone(), entities.dir.clone()));
        }
    }
    for (adapter, dir) in entity_dirs {
        let dir_rel = dir.trim_end_matches('/').to_string();
        let dir_abs = root.join(&dir_rel);
        if !dir_abs.is_dir() {
            errors.push(ValidationError::new(
                "missing-entities-dir",
                &adapter,
                format!("entities dir \"{dir}\" not found"),
                None,
            ));
            continue;
        }
        for file in toml_files(&dir_abs) {
            let rel = format!("{dir_rel}/{file}");
            let Some(entity) = read_toml::<EntityFile>(&dir_abs.join(&file), &rel, &mut errors)
            else {
                continue;
            };
            let name = file.trim_end_matches(".toml").to_string();
            check_schema_version(entity.schema, &name, &rel, &mut errors);
            house.entities.push(LoadedEntity {
                name,
                file: entity,
                path: rel,
                adapter: adapter.clone(),
            });
        }
    }

    (house, errors)
}

fn toml_files(dir: &Path) -> Vec<String> {
    let mut files: Vec<String> = fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|e| e.path().is_file())
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| n.ends_with(".toml"))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files
}

fn read_toml<T: serde::de::DeserializeOwned>(
    path: &Path,
    rel: &str,
    errors: &mut Vec<ValidationError>,
) -> Option<T> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) => {
            errors.push(ValidationError::new("parse-error", rel, err.to_string(), None));
            return None;
        }
    };
    match toml::from_str(&text) {
        Ok(value) => Some(value),
        Err(err) => {
            errors.push(ValidationError::new(
                "parse-error",
                rel,
                err.message().to_string(),
                None,
            ));
            None
        }
    }
}

fn check_schema_version(schema: u32, subject: &str, rel: &str, errors: &mut Vec<ValidationError>) {
    if schema != SUPPORTED_SCHEMA {
        errors.push(ValidationError::new(
            "unsupported-schema",
            subject,
            format!("schema {schema} is not supported (expected {SUPPORTED_SCHEMA})"),
            Some(rel.to_string()),
        ));
    }
}
