//! Unit content hashing for plan/apply (see docs/design.md, step 5b).
//!
//! Two hashes decide "changed" cheaply before any semantic comparison:
//! - `manifest_hash`: sha256 of the manifest file bytes.
//! - `files_hash`: sha256 over the unit's non-manifest repo inputs — command
//!   tokens that resolve to files (the `uv run units/foo.py` script), an
//!   adapter's entity files, and `zones.toml` when any of the unit's key
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
    for entity in house.entities.iter().filter(|e| &e.adapter == name) {
        feed(&entity.path);
    }
    if unit_uses_zone {
        feed("zones.toml");
    }

    hasher.finalize().iter().map(|b| format!("{b:02x}")).collect()
}
