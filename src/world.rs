//! Reads the actual world from a running supervisor's bus, through the core's
//! last-value queryables: meta (manifest hashes and bytes, grants, applied
//! commit) and config (current parameter values). This is the same surface any
//! observer gets; there is no second channel. The supervisor publishes a
//! unit's meta entries with [`WorldUnit::meta_entries`], and [`read`] parses
//! them back with [`WorldUnit::from_meta`].

use std::collections::BTreeMap;

use zenoh::Session;

use crate::bus;
use crate::grants::Grant;
use crate::plan::{World, WorldUnit};

/// Opens a client session against a supervisor's endpoint. An unreachable
/// endpoint is an error, so it cannot pass for an empty world.
pub async fn connect(endpoint: &str) -> Result<Session, String> {
    zenoh::open(bus::connect_config(endpoint))
        .await
        .map_err(|e| format!("cannot reach a supervisor at {endpoint}: {e}"))
}

/// Reads the world from the supervisor's last-value queryables: every
/// unit's applied manifest and hashes, the applied grants and commit, and
/// the live parameter values.
pub async fn read(session: &Session, endpoint: &str) -> Result<World, String> {
    let mut world = World {
        label: endpoint.to_string(),
        live: true,
        ..World::default()
    };

    let replies = session
        .get("home/meta/**")
        .await
        .map_err(|e| format!("meta query failed: {e}"))?;
    let mut meta: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    while let Ok(reply) = replies.recv_async().await {
        let Ok(sample) = reply.result() else { continue };
        meta.insert(
            sample.key_expr().as_str().to_string(),
            sample.payload().to_bytes().to_vec(),
        );
    }
    if let Some(payload) = meta.get(bus::GRANTS_KEY) {
        world.grants = serde_json::from_slice::<Vec<Grant>>(payload)
            .map_err(|e| format!("world grant table does not parse: {e}"))?;
    }
    if let Some(payload) = meta.get(bus::APPLIED_COMMIT_KEY) {
        world.applied_commit = Some(String::from_utf8_lossy(payload).to_string());
    }
    for key in meta.keys() {
        let Some((name, _)) = key
            .strip_prefix("home/meta/")
            .and_then(|rest| rest.split_once('/'))
        else {
            continue;
        };
        if let Some(unit) = WorldUnit::from_meta(name, &meta) {
            world.units.insert(name.to_string(), unit);
        }
    }

    let replies = session
        .get("home/config/*/*")
        .await
        .map_err(|e| format!("config query failed: {e}"))?;
    while let Ok(reply) = replies.recv_async().await {
        let Ok(sample) = reply.result() else { continue };
        let key = sample.key_expr().as_str().to_string();
        let segments: Vec<&str> = key.split('/').collect();
        if let ["home", "config", unit, param] = segments[..] {
            if let Ok(value) = serde_json::from_slice(&sample.payload().to_bytes()) {
                world
                    .params
                    .insert((unit.to_string(), param.to_string()), value);
            }
        }
    }

    Ok(world)
}

impl WorldUnit {
    /// The unit's keys in the meta space, in publish order:
    /// `manifest_hash`, `files_hash`, `manifest`.
    pub fn meta_keys(name: &str) -> [String; 3] {
        [
            bus::manifest_hash_key(name),
            bus::files_hash_key(name),
            bus::manifest_key(name),
        ]
    }

    /// The unit's meta entries as the supervisor publishes and serves them:
    /// each of [`WorldUnit::meta_keys`] with its payload.
    pub fn meta_entries(&self, name: &str) -> [(String, Vec<u8>); 3] {
        let [hash, files, manifest] = Self::meta_keys(name);
        [
            (hash, self.manifest_hash.clone().into_bytes()),
            (files, self.files_hash.clone().into_bytes()),
            (manifest, self.manifest.clone()),
        ]
    }

    /// Reads unit `name` back from the meta space, the inverse of
    /// [`WorldUnit::meta_entries`]. A missing entry reads as empty. `None`
    /// when none of the unit's entries is there, so a stray `log` reply does
    /// not create a unit.
    pub fn from_meta(name: &str, meta: &BTreeMap<String, Vec<u8>>) -> Option<WorldUnit> {
        let [hash, files, manifest] = Self::meta_keys(name).map(|key| meta.get(&key));
        if hash.is_none() && files.is_none() && manifest.is_none() {
            return None;
        }
        let text = |payload: Option<&Vec<u8>>| {
            payload.map_or_else(String::new, |p| String::from_utf8_lossy(p).to_string())
        };
        Some(WorldUnit {
            manifest: manifest.cloned().unwrap_or_default(),
            manifest_hash: text(hash),
            files_hash: text(files),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_unit_reads_back_from_its_meta_entries_and_a_log_alone_is_no_unit() {
        let unit = WorldUnit {
            manifest: b"schema = 1\n".to_vec(),
            manifest_hash: "abc".to_string(),
            files_hash: "def".to_string(),
        };
        let mut meta: BTreeMap<String, Vec<u8>> = unit.meta_entries("porch").into_iter().collect();
        meta.insert(bus::log_key("hall"), b"[]".to_vec());

        let read = WorldUnit::from_meta("porch", &meta).expect("porch is a unit");
        assert_eq!(read.manifest, unit.manifest);
        assert_eq!(read.manifest_hash, unit.manifest_hash);
        assert_eq!(read.files_hash, unit.files_hash);
        assert!(WorldUnit::from_meta("hall", &meta).is_none());
    }
}
