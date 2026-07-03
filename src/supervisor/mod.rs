//! The process supervisor behind `homeostat up`: spawns every unit in the
//! house, publishes manifest hashes to the meta key space, and shuts the
//! whole tree down gracefully on SIGTERM/SIGINT.

pub mod backoff;
pub mod process;
pub mod unit;

use std::fs;
use std::path::Path;

use sha2::{Digest, Sha256};
use tokio::sync::watch;

use crate::bus;
use crate::repo::House;
use crate::supervisor::unit::UnitSpec;

/// Runs the supervisor until SIGTERM/SIGINT. Assumes the house already
/// passed plan-time validation.
pub async fn run(house: &House, root: &Path, listen: &str) -> Result<(), String> {
    let session = zenoh::open(bus::listen_config(listen))
        .await
        .map_err(|e| format!("failed to open bus session on {listen}: {e}"))?;
    println!("[homeostat] bus listening on {listen}");

    for unit in &house.units {
        let manifest_bytes = fs::read(root.join(&unit.path))
            .map_err(|e| format!("failed to read {}: {e}", unit.path))?;
        let hash: String = Sha256::digest(&manifest_bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        session
            .put(bus::manifest_hash_key(&unit.manifest.unit.name), hash)
            .await
            .map_err(|e| format!("failed to publish manifest hash: {e}"))?;
    }

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let mut tasks = Vec::new();
    for unit in &house.units {
        let spec = UnitSpec {
            name: unit.manifest.unit.name.clone(),
            command: unit.manifest.runtime.command.clone(),
            restart: unit.manifest.runtime.restart,
            grace: UnitSpec::grace_from_manifest(unit.manifest.runtime.shutdown_grace_s),
            cwd: root.to_path_buf(),
            endpoint: listen.to_string(),
        };
        tasks.push(tokio::spawn(unit::supervise(
            spec,
            session.clone(),
            shutdown_rx.clone(),
        )));
    }

    wait_for_signal().await;
    println!("[homeostat] shutting down");
    let _ = shutdown_tx.send(true);
    for task in tasks {
        let _ = task.await;
    }
    let _ = session.close().await;
    Ok(())
}

async fn wait_for_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}
