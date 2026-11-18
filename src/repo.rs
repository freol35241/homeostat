//! Loading a house repo from disk into a [`House`]: every unit manifest, the
//! entity files each binding unit points at, `zones.toml` and
//! `dashboard.toml`. A file that fails to parse is reported and skipped, so
//! one bad file does not hide the errors in the others.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::error::{Code, Diagnostics};
use crate::manifest::{
    DashboardFile, EntityFile, UnitKind, UnitManifest, ZonesFile, SUPPORTED_SCHEMA,
};

/// A unit manifest and where it was read from.
#[derive(Debug)]
pub struct LoadedUnit {
    pub manifest: UnitManifest,
    /// House-relative path, e.g. `units/zigbee.toml`.
    pub path: String,
}

/// An entity file, its name, and the unit that binds it.
#[derive(Debug)]
pub struct LoadedEntity {
    /// The globally unique entity name: the file stem of the entity file.
    pub name: String,
    pub file: EntityFile,
    /// House-relative path, e.g. `entities/zigbee/kitchen_ceiling.toml`.
    pub path: String,
    /// Name of the unit whose entities dir this file lives in (an adapter,
    /// or an automation for virtual sensors).
    pub owner: String,
}

/// A house repo as loaded: units, entities, zones and the dashboard file.
#[derive(Debug, Default)]
pub struct House {
    pub units: Vec<LoadedUnit>,
    pub entities: Vec<LoadedEntity>,
    pub zones: BTreeMap<String, Vec<String>>,
    /// `dashboard.toml`, when the house has one.
    pub dashboard: Option<DashboardFile>,
}

impl House {
    /// The unit named `name`, if the house has one.
    pub fn unit(&self, name: &str) -> Option<&LoadedUnit> {
        self.units.iter().find(|u| u.manifest.unit.name == name)
    }

    /// All rooms that house at least one entity.
    pub fn rooms(&self) -> Vec<&str> {
        let mut rooms: Vec<&str> = self
            .entities
            .iter()
            .map(|e| e.file.entity.room.as_str())
            .collect();
        rooms.sort_unstable();
        rooms.dedup();
        rooms
    }
}

/// Reads a house repo. A parse failure does not abort the walk. The result is
/// a typed model of everything that parsed, and `diags` gets every error.
pub fn load(root: &Path, diags: &mut Diagnostics) -> House {
    let mut house = House::default();

    let zones_path = root.join("zones.toml");
    if zones_path.exists() {
        if let Some(zones) = read_toml::<ZonesFile>(&zones_path, "zones.toml", diags) {
            check_schema_version(zones.schema, "zones", "zones.toml", diags);
            house.zones = zones.zones;
        }
    }

    let dashboard_path = root.join("dashboard.toml");
    if dashboard_path.exists() {
        if let Some(dashboard) =
            read_toml::<DashboardFile>(&dashboard_path, "dashboard.toml", diags)
        {
            check_schema_version(dashboard.schema, "dashboard", "dashboard.toml", diags);
            house.dashboard = Some(dashboard);
        }
    }

    let units_dir = root.join("units");
    if !units_dir.is_dir() {
        diags.error(Code::MissingUnitsDir, "units", "directory not found");
        return house;
    }
    for file in toml_files(&units_dir) {
        let rel = format!("units/{file}");
        let Some(text) = read_text(&units_dir.join(&file), &rel, diags) else {
            continue;
        };
        house.units.extend(parse_unit(rel, &text, diags));
    }

    let mut entity_dirs: Vec<(String, String)> = Vec::new(); // (owner, dir)
    for unit in &house.units {
        if !matches!(
            unit.manifest.unit.kind,
            UnitKind::Adapter | UnitKind::Automation
        ) {
            continue;
        }
        if let Some(entities) = &unit.manifest.entities {
            entity_dirs.push((unit.manifest.unit.name.clone(), entities.dir.clone()));
        }
    }
    for (owner, dir) in entity_dirs {
        let dir_rel = dir.trim_end_matches('/').to_string();
        let dir_abs = root.join(&dir_rel);
        if !dir_abs.is_dir() {
            diags.error(
                Code::MissingEntitiesDir,
                &owner,
                format!("entities dir \"{dir}\" not found"),
            );
            continue;
        }
        for file in toml_files(&dir_abs) {
            let rel = format!("{dir_rel}/{file}");
            let Some(text) = read_text(&dir_abs.join(&file), &rel, diags) else {
                continue;
            };
            house
                .entities
                .extend(parse_entity(rel, &owner, &text, diags));
        }
    }

    house
}

/// Parses the unit manifest at house-relative `path`, adds the params every
/// unit of its kind has, and checks what one file can show: its schema
/// version, and that its file stem is its unit name.
fn parse_unit(path: String, text: &str, diags: &mut Diagnostics) -> Option<LoadedUnit> {
    let mut manifest = parse_toml::<UnitManifest>(text, &path, diags)?;
    manifest.add_implicit_params();
    check_schema_version(manifest.schema, &manifest.unit.name, &path, diags);
    if file_stem(&path) != manifest.unit.name {
        diags.in_file(&path).error(
            Code::UnitNameMismatch,
            &manifest.unit.name,
            format!(
                "manifest file is {path} but [unit] name is \"{}\"; the SDK reads units/{}.toml",
                manifest.unit.name, manifest.unit.name
            ),
        );
    }
    Some(LoadedUnit { manifest, path })
}

/// Parses the entity file at house-relative `path`, bound by unit `owner`.
/// The file stem is the entity's name.
fn parse_entity(
    path: String,
    owner: &str,
    text: &str,
    diags: &mut Diagnostics,
) -> Option<LoadedEntity> {
    let file = parse_toml::<EntityFile>(text, &path, diags)?;
    let name = file_stem(&path).to_string();
    check_schema_version(file.schema, &name, &path, diags);
    Some(LoadedEntity {
        name,
        file,
        path,
        owner: owner.to_string(),
    })
}

/// The file name in `path` without its `.toml` suffix.
fn file_stem(path: &str) -> &str {
    let file = path.rsplit_once('/').map_or(path, |(_, file)| file);
    file.strip_suffix(".toml").unwrap_or(file)
}

#[cfg(test)]
impl House {
    /// Builds a house from file contents, parsed and checked as [`load`]
    /// parses and checks each file it reads. `units` are (file stem,
    /// manifest) pairs, read as `units/{stem}.toml`. `entities` are (owner,
    /// name, entity file) triples, read as `entities/{owner}/{name}.toml`.
    /// Files keep the order given, where `load` sorts units by file name and
    /// entities by owner, then file name. An entity's owner is taken as
    /// given: unlike `load`, this does not read it from the owner's
    /// `[entities]` table.
    ///
    /// Panics on any error `load` would report for these files.
    pub(crate) fn from_toml(units: &[(&str, &str)], entities: &[(&str, &str, &str)]) -> House {
        let mut house = House::default();
        let mut diags = Diagnostics::default();
        for (stem, text) in units {
            house
                .units
                .extend(parse_unit(format!("units/{stem}.toml"), text, &mut diags));
        }
        for (owner, name, text) in entities {
            let path = format!("entities/{owner}/{name}.toml");
            house
                .entities
                .extend(parse_entity(path, owner, text, &mut diags));
        }
        assert!(diags.errors.is_empty(), "{:?}", diags.errors);
        house
    }
}

fn toml_files(dir: &Path) -> Vec<String> {
    let mut files: Vec<String> = fs::read_dir(dir).map_or_default(|entries| {
        entries
            .filter_map(std::result::Result::ok)
            .filter(|e| e.path().is_file())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.ends_with(".toml"))
            .collect()
    });
    files.sort();
    files
}

fn read_toml<T: serde::de::DeserializeOwned>(
    path: &Path,
    rel: &str,
    diags: &mut Diagnostics,
) -> Option<T> {
    let text = read_text(path, rel, diags)?;
    parse_toml(&text, rel, diags)
}

fn read_text(path: &Path, rel: &str, diags: &mut Diagnostics) -> Option<String> {
    match fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(err) => {
            diags.error(Code::ParseError, rel, err.to_string());
            None
        }
    }
}

fn parse_toml<T: serde::de::DeserializeOwned>(
    text: &str,
    rel: &str,
    diags: &mut Diagnostics,
) -> Option<T> {
    match toml::from_str(text) {
        Ok(value) => Some(value),
        Err(err) => {
            diags.error(Code::ParseError, rel, err.message());
            None
        }
    }
}

fn check_schema_version(schema: u32, subject: &str, rel: &str, diags: &mut Diagnostics) {
    if schema != SUPPORTED_SCHEMA {
        diags.in_file(rel).error(
            Code::UnsupportedSchema,
            subject,
            format!("schema {schema} is not supported (expected {SUPPORTED_SCHEMA})"),
        );
    }
}
