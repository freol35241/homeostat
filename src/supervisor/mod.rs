//! The process supervisor behind `homeostat up`: spawns every unit in the
//! house, publishes manifest hashes to the meta key space, serves the core
//! last-value cache (config, health, clock), and shuts the whole tree down
//! gracefully on SIGTERM/SIGINT.

pub mod backoff;
pub mod process;
pub mod unit;

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};
use tokio::sync::watch;
use zenoh::Session;

use crate::bus::{self, Health, HealthStatus};
use crate::config::ConfigStore;
use crate::repo::House;
use crate::supervisor::unit::UnitSpec;

/// Current health per unit, shared between the supervision tasks (writers)
/// and the health queryable (reader). This is the last-value cache that
/// lets late subscribers see current state without a republish loop.
pub type HealthMap = Arc<Mutex<BTreeMap<String, Health>>>;

/// Runs the supervisor until SIGTERM/SIGINT. Assumes the house already
/// passed plan-time validation.
pub async fn run(house: &House, root: &Path, listen: &str) -> Result<(), String> {
    let session = zenoh::open(bus::listen_config(listen))
        .await
        .map_err(|e| format!("failed to open bus session on {listen}: {e}"))?;
    println!("[homeostat] bus listening on {listen}");

    // The last-value queryables are up before any unit spawns, so a unit's
    // first get always finds them.
    let store = Arc::new(ConfigStore::from_house(house));
    serve_config(&session, store).await?;
    let health: HealthMap = Arc::new(Mutex::new(
        house
            .units
            .iter()
            .map(|u| (u.manifest.unit.name.clone(), initial_health()))
            .collect(),
    ));
    serve_health(&session, health.clone()).await?;
    mirror_clock(&session).await?;

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
            health.clone(),
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

fn initial_health() -> Health {
    Health {
        status: HealthStatus::Starting,
        pid: None,
        restarts: 0,
        backoff_ms: None,
        last_exit_code: None,
    }
}

/// Whether a query's selector covers a concrete key.
fn intersects(query: &zenoh::query::Query, key: &str) -> bool {
    zenoh::key_expr::KeyExpr::try_from(key.to_string())
        .map(|k| query.key_expr().intersects(&k))
        .unwrap_or(false)
}

/// Serves `home/config/{unit}/{param}`: GET without payload reads the
/// current value, GET with payload is a validated write (see src/config.rs).
async fn serve_config(session: &Session, store: Arc<ConfigStore>) -> Result<(), String> {
    let queryable = session
        .declare_queryable("home/config/*/*")
        .await
        .map_err(|e| format!("failed to declare config queryable: {e}"))?;
    let session = session.clone();
    tokio::spawn(async move {
        while let Ok(query) = queryable.recv_async().await {
            handle_config_query(&store, &session, query).await;
        }
    });
    Ok(())
}

async fn handle_config_query(store: &ConfigStore, session: &Session, query: zenoh::query::Query) {
    let Some(payload) = query.payload() else {
        // Read: reply the current value of every parameter the selector covers.
        for (unit, param, value) in
            store.read(|unit, param| intersects(&query, &bus::config_key(unit, param)))
        {
            let _ = query
                .reply(bus::config_key(&unit, &param), value.to_string())
                .await;
        }
        return;
    };

    // Write request: exactly one concrete parameter key.
    let key = query.key_expr().as_str().to_string();
    let segments: Vec<&str> = key.split('/').collect();
    let (unit, param) = match segments[..] {
        ["home", "config", unit, param]
            if !unit.contains('*') && !param.contains('*') =>
        {
            (unit, param)
        }
        _ => {
            reply_config_err(&query, "a write must target home/config/{unit}/{param}").await;
            return;
        }
    };
    let value: serde_json::Value = match serde_json::from_slice(&payload.to_bytes()) {
        Ok(value) => value,
        Err(_) => {
            reply_config_err(&query, "payload is not JSON").await;
            return;
        }
    };
    match store.write(unit, param, value) {
        Ok(stored) => {
            let text = stored.to_string();
            let _ = session.put(&key, text.clone()).await;
            let _ = query.reply(key, text).await;
        }
        Err(message) => reply_config_err(&query, &message).await,
    }
}

async fn reply_config_err(query: &zenoh::query::Query, message: &str) {
    let payload = serde_json::json!({ "error": message }).to_string();
    let _ = query.reply_err(payload).await;
}

/// Serves current health at `home/health/{unit}` to late joiners; the
/// supervision tasks publish transitions and keep the map current.
async fn serve_health(session: &Session, health: HealthMap) -> Result<(), String> {
    let queryable = session
        .declare_queryable("home/health/*")
        .await
        .map_err(|e| format!("failed to declare health queryable: {e}"))?;
    tokio::spawn(async move {
        while let Ok(query) = queryable.recv_async().await {
            let entries: Vec<(String, Health)> = health
                .lock()
                .expect("health map lock")
                .iter()
                .map(|(unit, h)| (unit.clone(), h.clone()))
                .collect();
            for (unit, h) in entries {
                let key = bus::health_key(&unit);
                if intersects(&query, &key) {
                    let payload = serde_json::to_string(&h).expect("health serializes");
                    let _ = query.reply(key, payload).await;
                }
            }
        }
    });
    Ok(())
}

/// Mirrors `home/clock/*` into a last-value cache served by a queryable, so
/// a late joiner sees the current minute/date instead of waiting out the
/// next boundary.
async fn mirror_clock(session: &Session) -> Result<(), String> {
    let cache: Arc<Mutex<BTreeMap<String, Vec<u8>>>> = Arc::default();
    let sub = session
        .declare_subscriber("home/clock/*")
        .await
        .map_err(|e| format!("failed to subscribe to clock keys: {e}"))?;
    let queryable = session
        .declare_queryable("home/clock/*")
        .await
        .map_err(|e| format!("failed to declare clock queryable: {e}"))?;
    {
        let cache = cache.clone();
        tokio::spawn(async move {
            while let Ok(sample) = sub.recv_async().await {
                cache.lock().expect("clock cache lock").insert(
                    sample.key_expr().as_str().to_string(),
                    sample.payload().to_bytes().to_vec(),
                );
            }
        });
    }
    tokio::spawn(async move {
        while let Ok(query) = queryable.recv_async().await {
            let entries: Vec<(String, Vec<u8>)> = cache
                .lock()
                .expect("clock cache lock")
                .iter()
                .filter(|(key, _)| intersects(&query, key))
                .map(|(key, payload)| (key.clone(), payload.clone()))
                .collect();
            for (key, payload) in entries {
                let _ = query.reply(key, payload).await;
            }
        }
    });
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
