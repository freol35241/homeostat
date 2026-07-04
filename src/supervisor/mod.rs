//! The process supervisor behind `homeostat up`: spawns every unit in the
//! house, serves the core last-value caches (config, health, clock, state,
//! meta), executes apply walks commanded over the bus, and shuts the whole
//! tree down gracefully on SIGTERM/SIGINT.

pub mod apply;
pub mod backoff;
pub mod process;
pub mod unit;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::watch;
use zenoh::Session;

use crate::bus::{self, Health, HealthStatus};
use crate::config::ConfigStore;
use crate::grants::Grant;
use crate::plan::WorldUnit;
use crate::supervisor::unit::UnitSpec;
use crate::CheckResult;

/// Current health per unit, shared between the supervision tasks (writers)
/// and the health queryable (reader). This is the last-value cache that
/// lets late subscribers see current state without a republish loop.
pub type HealthMap = Arc<Mutex<BTreeMap<String, Health>>>;

/// How long an apply step waits for a (re)started unit to reach `running`
/// before halting the walk. Breaker-open and stopped halt sooner.
const READY_DEADLINE: Duration = Duration::from_secs(60);

/// What the supervisor knows to be applied: the world it serves at
/// `home/meta/**`. Unit entries update only when a unit reaches `running`
/// during an apply (or at startup), so a halted walk re-plans the
/// remaining work instead of pretending it landed.
#[derive(Default)]
pub struct WorldMeta {
    pub units: BTreeMap<String, WorldUnit>,
    pub grants: Vec<Grant>,
    pub applied_commit: Option<String>,
}

struct UnitHandle {
    shutdown: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

/// Shared state of a running supervisor: everything the queryables and the
/// apply engine touch.
pub struct Core {
    pub session: Session,
    pub root: PathBuf,
    pub listen: String,
    pub store: Arc<ConfigStore>,
    pub health: HealthMap,
    world: Mutex<WorldMeta>,
    units: tokio::sync::Mutex<BTreeMap<String, UnitHandle>>,
    apply_lock: tokio::sync::Mutex<()>,
}

/// Runs the supervisor until SIGTERM/SIGINT. Assumes the house already
/// passed plan-time validation.
pub async fn run(check: &CheckResult, root: &Path, listen: &str) -> Result<(), String> {
    let session = zenoh::open(bus::listen_config(listen))
        .await
        .map_err(|e| format!("failed to open bus session on {listen}: {e}"))?;
    println!("[homeostat] bus listening on {listen}");

    // The last-value queryables are up before any unit spawns, so a unit's
    // first get always finds them.
    let store = Arc::new(ConfigStore::from_house(&check.house));
    serve_config(&session, store.clone()).await?;
    let health: HealthMap = Arc::default();
    serve_health(&session, health.clone()).await?;
    mirror(&session, "home/clock/*").await?;
    mirror(&session, "home/state/**").await?;

    let mut world = WorldMeta { grants: check.grants.clone(), ..WorldMeta::default() };
    for unit in &check.house.units {
        world.units.insert(
            unit.manifest.unit.name.clone(),
            crate::plan::world_unit_from_repo(root, unit, &check.house, &check.expanded),
        );
    }

    let core = Arc::new(Core {
        session: session.clone(),
        root: root.to_path_buf(),
        listen: listen.to_string(),
        store,
        health,
        world: Mutex::new(world),
        units: tokio::sync::Mutex::new(BTreeMap::new()),
        apply_lock: tokio::sync::Mutex::new(()),
    });
    serve_meta(core.clone()).await?;
    apply::serve(core.clone()).await?;
    core.publish_meta().await;

    for unit in &check.house.units {
        core.launch(UnitSpec::from_loaded(unit, root, listen)).await;
    }

    wait_for_signal().await;
    println!("[homeostat] shutting down");
    let handles: Vec<UnitHandle> = {
        let mut units = core.units.lock().await;
        std::mem::take(&mut *units).into_values().collect()
    };
    for handle in &handles {
        let _ = handle.shutdown.send(true);
    }
    for handle in handles {
        let _ = handle.task.await;
    }
    let _ = session.close().await;
    Ok(())
}

impl Core {
    /// The world as this supervisor would report it over the bus.
    pub fn snapshot(&self) -> crate::plan::World {
        let world = self.world.lock().expect("world meta lock");
        crate::plan::World {
            label: self.listen.clone(),
            live: true,
            units: world.units.clone(),
            params: self
                .store
                .read(|_, _| true)
                .into_iter()
                .map(|(unit, param, value)| ((unit, param), value))
                .collect(),
            grants: world.grants.clone(),
            applied_commit: world.applied_commit.clone(),
        }
    }

    /// Spawns a fresh supervision task for `spec`. The health entry is set
    /// to `starting` synchronously so a reader never sees the previous
    /// incarnation's terminal state after this returns.
    async fn launch(&self, spec: UnitSpec) {
        let name = spec.name.clone();
        self.health
            .lock()
            .expect("health map lock")
            .insert(name.clone(), initial_health());
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(unit::supervise(
            spec,
            self.session.clone(),
            self.health.clone(),
            shutdown_rx,
        ));
        self.units
            .lock()
            .await
            .insert(name, UnitHandle { shutdown: shutdown_tx, task });
    }

    /// Stops a unit's supervision task (graceful per the unit contract) and
    /// waits for it to finish. No-op when the unit is not running.
    async fn stop(&self, name: &str) {
        let handle = self.units.lock().await.remove(name);
        if let Some(handle) = handle {
            let _ = handle.shutdown.send(true);
            let _ = handle.task.await;
        }
    }

    /// Stops a destroyed unit and removes every trace: health entry, meta
    /// entries, world membership.
    async fn destroy(&self, name: &str) {
        self.stop(name).await;
        self.health.lock().expect("health map lock").remove(name);
        self.world.lock().expect("world meta lock").units.remove(name);
        for key in [
            bus::manifest_hash_key(name),
            bus::files_hash_key(name),
            bus::manifest_key(name),
        ] {
            let _ = self.session.delete(key).await;
        }
    }

    /// Waits until the unit's liveliness token is up (health `running`).
    /// Halts early when the breaker opens or the unit stops for good.
    async fn await_ready(&self, name: &str) -> Result<(), String> {
        let deadline = tokio::time::Instant::now() + READY_DEADLINE;
        loop {
            let status = self
                .health
                .lock()
                .expect("health map lock")
                .get(name)
                .map(|h| h.status);
            match status {
                Some(HealthStatus::Running) => return Ok(()),
                Some(HealthStatus::Open) => {
                    return Err("circuit breaker open".to_string());
                }
                Some(HealthStatus::Stopped) => {
                    return Err("stopped before becoming ready".to_string());
                }
                _ => {}
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(format!("not running within {}s", READY_DEADLINE.as_secs()));
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Records a unit as applied and publishes its meta keys.
    async fn record_unit(&self, name: &str, unit: WorldUnit) {
        let manifest = unit.manifest.clone();
        let manifest_hash = unit.manifest_hash.clone();
        let files_hash = unit.files_hash.clone();
        self.world
            .lock()
            .expect("world meta lock")
            .units
            .insert(name.to_string(), unit);
        let _ = self.session.put(bus::manifest_hash_key(name), manifest_hash).await;
        let _ = self.session.put(bus::files_hash_key(name), files_hash).await;
        let _ = self.session.put(bus::manifest_key(name), manifest).await;
    }

    async fn record_grants(&self, grants: Vec<Grant>) {
        let payload = serde_json::to_string(&grants).expect("grants serialize");
        self.world.lock().expect("world meta lock").grants = grants;
        let _ = self.session.put(bus::GRANTS_KEY, payload).await;
    }

    async fn record_applied_commit(&self, commit: String) {
        self.world.lock().expect("world meta lock").applied_commit = Some(commit.clone());
        let _ = self.session.put(bus::APPLIED_COMMIT_KEY, commit).await;
    }

    /// Publishes the whole meta space (startup).
    async fn publish_meta(&self) {
        let (units, grants): (Vec<(String, WorldUnit)>, Vec<Grant>) = {
            let world = self.world.lock().expect("world meta lock");
            (
                world.units.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                world.grants.clone(),
            )
        };
        for (name, unit) in units {
            let _ = self.session.put(bus::manifest_hash_key(&name), unit.manifest_hash).await;
            let _ = self.session.put(bus::files_hash_key(&name), unit.files_hash).await;
            let _ = self.session.put(bus::manifest_key(&name), unit.manifest).await;
        }
        let payload = serde_json::to_string(&grants).expect("grants serialize");
        let _ = self.session.put(bus::GRANTS_KEY, payload).await;
    }
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

/// Serves the meta space to late joiners: manifest hashes and bytes per
/// unit, the resolved grant table, and the applied commit. This is what
/// `homeostat plan --bus` reads as the world.
async fn serve_meta(core: Arc<Core>) -> Result<(), String> {
    let queryable = core
        .session
        .declare_queryable("home/meta/**")
        .await
        .map_err(|e| format!("failed to declare meta queryable: {e}"))?;
    tokio::spawn(async move {
        while let Ok(query) = queryable.recv_async().await {
            let entries: Vec<(String, Vec<u8>)> = {
                let world = core.world.lock().expect("world meta lock");
                let mut entries = Vec::new();
                for (name, unit) in &world.units {
                    entries.push((
                        bus::manifest_hash_key(name),
                        unit.manifest_hash.clone().into_bytes(),
                    ));
                    entries.push((
                        bus::files_hash_key(name),
                        unit.files_hash.clone().into_bytes(),
                    ));
                    entries.push((bus::manifest_key(name), unit.manifest.clone()));
                }
                entries.push((
                    bus::GRANTS_KEY.to_string(),
                    serde_json::to_vec(&world.grants).expect("grants serialize"),
                ));
                if let Some(commit) = &world.applied_commit {
                    entries.push((
                        bus::APPLIED_COMMIT_KEY.to_string(),
                        commit.clone().into_bytes(),
                    ));
                }
                entries
            };
            for (key, payload) in entries {
                if intersects(&query, &key) {
                    let _ = query.reply(key, payload).await;
                }
            }
        }
    });
    Ok(())
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

/// Mirrors a published key space into a last-value cache served by a
/// queryable. Clock: a late joiner sees the current minute/date instead of
/// waiting out the next boundary. State: a late joiner (or a bus read, e.g.
/// the MCP surface's read_state) sees every entity's current value without
/// waiting for the next publish.
async fn mirror(session: &Session, keyexpr: &'static str) -> Result<(), String> {
    let cache: Arc<Mutex<BTreeMap<String, Vec<u8>>>> = Arc::default();
    let sub = session
        .declare_subscriber(keyexpr)
        .await
        .map_err(|e| format!("failed to subscribe to {keyexpr}: {e}"))?;
    let queryable = session
        .declare_queryable(keyexpr)
        .await
        .map_err(|e| format!("failed to declare {keyexpr} queryable: {e}"))?;
    {
        let cache = cache.clone();
        tokio::spawn(async move {
            while let Ok(sample) = sub.recv_async().await {
                cache.lock().expect("mirror cache lock").insert(
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
                .expect("mirror cache lock")
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
