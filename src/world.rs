//! Reads the actual world from a running supervisor's bus, through the
//! core's last-value queryables — meta (manifest hashes and bytes, grants,
//! applied commit) and config (current parameter values). No second
//! channel: this is the same surface any observer gets.

use zenoh::Session;

use crate::bus;
use crate::grants::Grant;
use crate::plan::{World, WorldUnit};

/// Opens a client session against a supervisor's endpoint. An unreachable
/// endpoint is a hard error — never a silent empty world.
pub async fn connect(endpoint: &str) -> Result<Session, String> {
    zenoh::open(bus::connect_config(endpoint))
        .await
        .map_err(|e| format!("cannot reach a supervisor at {endpoint}: {e}"))
}

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
    while let Ok(reply) = replies.recv_async().await {
        let Ok(sample) = reply.result() else { continue };
        let key = sample.key_expr().as_str().to_string();
        let payload = sample.payload().to_bytes().to_vec();
        let segments: Vec<&str> = key.split('/').collect();
        match segments[..] {
            ["home", "meta", "system", "grants"] => {
                world.grants = serde_json::from_slice::<Vec<Grant>>(&payload)
                    .map_err(|e| format!("world grant table does not parse: {e}"))?;
            }
            ["home", "meta", "system", "applied_commit"] => {
                world.applied_commit =
                    Some(String::from_utf8_lossy(&payload).to_string());
            }
            ["home", "meta", unit, field] => {
                let entry = world.units.entry(unit.to_string()).or_insert_with(|| WorldUnit {
                    manifest: Vec::new(),
                    manifest_hash: String::new(),
                    files_hash: String::new(),
                });
                match field {
                    "manifest" => entry.manifest = payload,
                    "manifest_hash" => {
                        entry.manifest_hash = String::from_utf8_lossy(&payload).to_string()
                    }
                    "files_hash" => {
                        entry.files_hash = String::from_utf8_lossy(&payload).to_string()
                    }
                    _ => {}
                }
            }
            _ => {}
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
