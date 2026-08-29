//! Unit content hashing for plan/apply (see docs/design.md, step 5b).
//!
//! Two hashes decide "changed" cheaply before any semantic comparison:
//! - `manifest_hash`: sha256 of the manifest file bytes.
//! - `files_hash`: sha256 over the unit's non-manifest repo inputs — command
//!   tokens that resolve to files (the `uv run units/foo.py` script), the
//!   unit's bound entity files, and `zones.toml` when any of the unit's key
//!   expressions referenced a zone.

use std::fs;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::expand::ExpandedKey;
use crate::repo::{House, LoadedUnit};

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

pub fn manifest_hash(manifest_bytes: &[u8]) -> String {
    sha256_hex(manifest_bytes)
}

/// Whether any of the unit's bus expressions expanded through a zone.
pub fn uses_zone(unit: &str, expanded: &[ExpandedKey]) -> bool {
    expanded.iter().any(|k| k.unit == unit && k.zone.is_some())
}

/// Hashes the unit's non-manifest inputs. Paths are house-root-relative and
/// fed into the hash alongside the content, so a rename is a change even
/// with identical bytes. A command token that does not resolve to a file
/// (program names on PATH, flags) contributes nothing.
pub fn files_hash(root: &Path, unit: &LoadedUnit, house: &House, unit_uses_zone: bool) -> String {
    let mut hasher = Sha256::new();
    let mut feed = |rel: &str| {
        let path = root.join(rel);
        if let Ok(bytes) = fs::read(&path) {
            hasher.update(rel.as_bytes());
            hasher.update([0u8]);
            hasher.update(&bytes);
            hasher.update([0u8]);
        }
    };

    for token in unit.manifest.runtime.command.split_whitespace() {
        if root.join(token).is_file() {
            feed(token);
        }
    }
    let name = &unit.manifest.unit.name;
    if unit.manifest.unit.inputs == Some(crate::manifest::UnitInputs::House) {
        // A house-wide unit reads every manifest, every entity file and
        // the zones: all of them are its inputs, so all of them must be
        // able to mark it changed. Sorted, since the hash is order-fed.
        let mut paths: Vec<&str> = house
            .units
            .iter()
            .map(|u| u.path.as_str())
            .chain(house.entities.iter().map(|e| e.path.as_str()))
            .collect();
        paths.sort_unstable();
        for path in paths {
            feed(path);
        }
        feed("zones.toml");
        return hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    }
    for entity in house.entities.iter().filter(|e| &e.owner == name) {
        feed(&entity.path);
    }
    if unit_uses_zone {
        feed("zones.toml");
    }

    hasher.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A minimal house: one adapter owning entities, one house-wide unit.
    fn house_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("homeostat-content-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("entities/probe")).unwrap();
        fs::create_dir_all(dir.join("units")).unwrap();
        fs::write(dir.join("zones.toml"), "schema = 1\n\n[zones]\n").unwrap();
        fs::write(
            dir.join("units/probe.toml"),
            "schema = 1\n\n[unit]\nname = \"probe\"\nkind = \"adapter\"\n\n\
             [runtime]\ncommand = \"fake_adapter\"\nrestart = \"always\"\n\n\
             [discovery]\nmode = \"static\"\nendpoint = \"fake://local\"\n\n\
             [entities]\ndir = \"entities/probe/\"\n",
        )
        .unwrap();
        fs::write(
            dir.join("units/dash.toml"),
            "schema = 1\n\n[unit]\nname = \"dash\"\nkind = \"service\"\ninputs = \"house\"\n\n\
             [runtime]\ncommand = \"dash\"\nrestart = \"always\"\n",
        )
        .unwrap();
        entity(&dir, "lamp");
        dir
    }

    fn entity(dir: &Path, name: &str) {
        fs::write(
            dir.join(format!("entities/probe/{name}.toml")),
            format!(
                "schema = 1\n\n[entity]\nid = \"{name}-1\"\ncapability = \"light\"\n\
                 room = \"den\"\n\n[write_policy]\nmode = \"shared\"\nowner = \"probe\"\n"
            ),
        )
        .unwrap();
    }

    fn hash_of(root: &Path, unit_name: &str) -> String {
        let (house, errors) = crate::repo::load(root);
        assert!(errors.is_empty(), "fixture must be valid: {errors:?}");
        let unit = house.unit(unit_name).expect("unit in fixture");
        files_hash(root, unit, &house, false)
    }

    #[test]
    fn a_house_wide_unit_is_changed_by_another_units_entity() {
        // The dashboard's model spans the whole house, so a binding added
        // to an adapter changes its inputs while changing none of its own
        // files. Without this, apply reports success and leaves the page
        // confidently stale.
        let dir = house_dir("house-scope");
        let before = hash_of(&dir, "dash");
        entity(&dir, "second_lamp");
        assert_ne!(hash_of(&dir, "dash"), before, "a new entity must reach it");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_house_wide_unit_is_changed_by_another_units_manifest() {
        let dir = house_dir("house-manifest");
        let before = hash_of(&dir, "dash");
        let probe = dir.join("units/probe.toml");
        let manifest = fs::read_to_string(&probe).unwrap();
        fs::write(&probe, manifest.replace("restart = \"always\"", "restart = \"on-failure\"")).unwrap();
        assert_ne!(hash_of(&dir, "dash"), before, "a manifest edit must reach it");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_own_scope_unit_ignores_files_it_does_not_own() {
        // The default stays narrow: probe owns its entities, and the
        // dashboard's manifest is none of its business.
        let dir = house_dir("own-scope");
        let before = hash_of(&dir, "probe");
        let dash = dir.join("units/dash.toml");
        let manifest = fs::read_to_string(&dash).unwrap();
        fs::write(&dash, manifest.replace("restart = \"always\"", "restart = \"on-failure\"")).unwrap();
        assert_eq!(hash_of(&dir, "probe"), before);
        entity(&dir, "third_lamp");
        assert_ne!(hash_of(&dir, "probe"), before, "its own entities still count");
        let _ = fs::remove_dir_all(&dir);
    }
}
