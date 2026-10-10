//! The process supervisor behind `homeostat up`: spawns every unit in the
//! house, serves the core last-value caches (config, health, clock, state,
//! meta), executes apply walks commanded over the bus, and shuts the whole
//! tree down gracefully on SIGTERM/SIGINT.

pub mod apply;
pub mod backoff;
pub mod mirror;
pub mod process;
pub mod replay;
pub mod unit;

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::watch;
use zenoh::Session;

use crate::bus::{self, Health, HealthStatus, LogEntry};
use crate::config::ConfigStore;
use crate::grants::Grant;
use crate::plan::WorldUnit;
use crate::supervisor::mirror::mirror;
use crate::supervisor::unit::UnitSpec;
use crate::CheckResult;

/// Current health per unit, shared between the supervision tasks (writers)
/// and the health queryable (reader). This is the last-value cache that
/// lets late subscribers see current state without a republish loop.
pub type HealthMap = Arc<Mutex<BTreeMap<String, Health>>>;

/// Captured stdout/stderr per unit, shared between the capture tasks (writers)
/// and the meta queryable (reader). This is the ring buffer served at
/// `home/meta/{unit}/log`. A unit with no captured output yet has no entry.
pub type LogMap = Arc<Mutex<BTreeMap<String, VecDeque<LogEntry>>>>;

/// How long an apply step waits for a (re)started unit to reach `running`
/// before halting the walk. Breaker-open and stopped halt sooner.
const READY_DEADLINE: Duration = Duration::from_mins(1);

/// What the supervisor knows to be applied: the world it serves at
/// `home/meta/**`. Unit entries update only when a unit reaches `running`
/// during an apply or at startup. A halted walk therefore re-plans the
/// remaining work instead of reporting it as done.
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
    pub log: LogMap,
    world: Mutex<WorldMeta>,
    units: tokio::sync::Mutex<BTreeMap<String, UnitHandle>>,
    apply_lock: tokio::sync::Mutex<()>,
    /// Set under the units lock when shutdown begins. `launch` then refuses
    /// and an in-flight apply walk halts, so no unit can enter the drained
    /// units map and miss the shutdown signal.
    shutting_down: AtomicBool,
}

/// Runs the supervisor until SIGTERM/SIGINT. Assumes the house already
/// passed plan-time validation.
pub async fn run(check: &CheckResult, root: &Path, listen: &str) -> Result<(), String> {
    // Install signal handlers before anything that takes time. The bus socket
    // accepts connections before `zenoh::open` returns. A SIGTERM that lands
    // while the core is still starting must take the graceful path below, and
    // not the default disposition that kills the process.
    let stop = stop_signal();
    let session = zenoh::open(bus::listen_config(listen))
        .await
        .map_err(|e| format!("failed to open bus session on {listen}: {e}"))?;
    println!("[homeostat] bus listening on {listen}");

    // The last-value queryables are up before any unit spawns, so a unit's
    // first get finds them.
    let store = Arc::new(ConfigStore::from_house(&check.house));
    serve_config(&session, store.clone()).await?;
    let health: HealthMap = Arc::default();
    serve_health(&session, health.clone()).await?;
    let log: LogMap = Arc::default();
    mirror(&session, "home/clock/*").await?;
    let state = mirror(&session, "home/state/**").await?;
    mirror(&session, "home/discovery/*").await?;
    // Forecasts are mirrored for the same reason as state. It matters more
    // here: a day-ahead curve is published once a day, so a consumer
    // restarting at midday would otherwise have no inputs until the next
    // morning. The reply's age is the mirror's, not the forecast's. A forecast
    // carries its own `issued`, and a consumer's staleness policy reads that
    // (docs/design.md#forecasts).
    mirror(&session, "home/forecast/**").await?;
    // The arbiter's holds, for the same late-joiner reason. A browser opening
    // during a hold must see it, and the arbiter republishes only on change
    // (docs/design.md#arbitrated-mode).
    mirror(&session, "home/hold/*").await?;

    let mut world = WorldMeta {
        grants: check.grants.clone(),
        ..WorldMeta::default()
    };
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
        log,
        world: Mutex::new(world),
        units: tokio::sync::Mutex::new(BTreeMap::new()),
        apply_lock: tokio::sync::Mutex::new(()),
        shutting_down: AtomicBool::new(false),
    });
    serve_meta(core.clone()).await?;
    apply::serve(core.clone()).await?;
    core.publish_meta().await;

    for unit in &check.house.units {
        core.launch(UnitSpec::from_loaded(unit, root, listen)).await;
    }
    if let Some(recorder) = recorder_unit(check) {
        tokio::spawn(replay::replay_when_running(core.clone(), recorder, state));
    }

    stop.await;
    println!("[homeostat] shutting down");
    let handles: Vec<UnitHandle> = {
        let mut units = core.units.lock().await;
        // Set under the lock. A concurrent launch either sees the flag and
        // refuses, or inserted before the drain and gets the signal.
        core.shutting_down.store(true, Ordering::SeqCst);
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
    /// Whether shutdown began; an in-flight apply walk halts on this.
    fn shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }

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

    /// Spawns a fresh supervision task for `spec`. The health entry is set to
    /// `starting` synchronously, so after this returns a reader does not see
    /// the previous incarnation's terminal state. A no-op once shutdown began,
    /// because a unit spawned into a closing supervisor would not receive the
    /// shutdown signal.
    async fn launch(&self, spec: UnitSpec) {
        let name = spec.name.clone();
        let mut units = self.units.lock().await;
        if self.shutting_down.load(Ordering::SeqCst) {
            return;
        }
        self.health
            .lock()
            .expect("health map lock")
            .insert(name.clone(), initial_health());
        // The log entry exists for the unit's whole lifetime, and capture only
        // appends to an existing entry. So a destroyed unit's final lines
        // cannot bring it back into the served meta space.
        self.log
            .lock()
            .expect("log map lock")
            .entry(name.clone())
            .or_default();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(unit::supervise(
            spec,
            self.session.clone(),
            self.health.clone(),
            self.log.clone(),
            shutdown_rx,
        ));
        units.insert(
            name,
            UnitHandle {
                shutdown: shutdown_tx,
                task,
            },
        );
    }

    /// Stops a unit's supervision task (gracefully, per the unit contract) and
    /// waits for it to finish. No-op when the unit is not running.
    async fn stop(&self, name: &str) {
        let handle = self.units.lock().await.remove(name);
        if let Some(handle) = handle {
            let _ = handle.shutdown.send(true);
            let _ = handle.task.await;
        }
    }

    /// Stops a destroyed unit and removes its health entry, meta entries and
    /// world membership.
    async fn destroy(&self, name: &str) {
        self.stop(name).await;
        self.health.lock().expect("health map lock").remove(name);
        self.log.lock().expect("log map lock").remove(name);
        self.world
            .lock()
            .expect("world meta lock")
            .units
            .remove(name);
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
        let _ = self
            .session
            .put(bus::manifest_hash_key(name), manifest_hash)
            .await;
        let _ = self
            .session
            .put(bus::files_hash_key(name), files_hash)
            .await;
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
                world
                    .units
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
                world.grants.clone(),
            )
        };
        for (name, unit) in units {
            let _ = self
                .session
                .put(bus::manifest_hash_key(&name), unit.manifest_hash)
                .await;
            let _ = self
                .session
                .put(bus::files_hash_key(&name), unit.files_hash)
                .await;
            let _ = self
                .session
                .put(bus::manifest_key(&name), unit.manifest)
                .await;
        }
        let payload = serde_json::to_string(&grants).expect("grants serialize");
        let _ = self.session.put(bus::GRANTS_KEY, payload).await;
    }
}

fn initial_health() -> Health {
    Health::idle(HealthStatus::Starting, 0, None)
}

/// Whether a query's selector covers a concrete key.
fn intersects(query: &zenoh::query::Query, key: &str) -> bool {
    zenoh::key_expr::KeyExpr::try_from(key.to_string())
        .is_ok_and(|k| query.key_expr().intersects(&k))
}

/// Serves the meta space to late joiners: manifest hashes and bytes per unit,
/// the resolved grant table, the applied commit, and `about`.
/// `homeostat plan --bus` reads this as the world.
async fn serve_meta(core: Arc<Core>) -> Result<(), String> {
    let queryable = core
        .session
        .declare_queryable("home/meta/**")
        .await
        .map_err(|e| format!("failed to declare meta queryable: {e}"))?;
    tokio::spawn(async move {
        while let Ok(query) = queryable.recv_async().await {
            let lines_cap = query
                .parameters()
                .get("lines")
                .and_then(|v| v.parse::<usize>().ok());
            let mut entries: Vec<(String, Vec<u8>)> = {
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
                entries.push((
                    bus::ABOUT_KEY.to_string(),
                    serde_json::to_vec(&bus::about(world.applied_commit.as_deref()))
                        .expect("about serializes"),
                ));
                entries
            };
            {
                // A unit with no captured output yet has no entry here, so a
                // query for it gets no reply, the same as an unknown unit's
                // manifest_hash above.
                let log = core.log.lock().expect("log map lock");
                for (name, buffer) in log.iter() {
                    let tail: Vec<&LogEntry> = match lines_cap {
                        Some(n) if n < buffer.len() => {
                            buffer.iter().skip(buffer.len() - n).collect()
                        }
                        _ => buffer.iter().collect(),
                    };
                    entries.push((
                        bus::log_key(name),
                        serde_json::to_vec(&tail).expect("log entries serialize"),
                    ));
                }
            }
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
        // Read: reply with the current value of every parameter the selector
        // covers.
        for (unit, param, value) in
            store.read(|unit, param| intersects(&query, &bus::config_key(unit, param)))
        {
            let _ = query
                .reply(bus::config_key(&unit, &param), value.to_string())
                .await;
        }
        return;
    };

    // Write request: one concrete parameter key.
    let key = query.key_expr().as_str().to_string();
    let segments: Vec<&str> = key.split('/').collect();
    let (unit, param) = match segments[..] {
        ["home", "config", unit, param] if !unit.contains('*') && !param.contains('*') => {
            (unit, param)
        }
        _ => {
            reply_config_err(&query, "a write must target home/config/{unit}/{param}").await;
            return;
        }
    };
    let value: serde_json::Value = if let Ok(value) = serde_json::from_slice(&payload.to_bytes()) {
        value
    } else {
        reply_config_err(&query, "payload is not JSON").await;
        return;
    };
    // Store mutation and bus put happen together, in order (see write_lock).
    let guard = store.write_lock().await;
    match store.write(unit, param, value) {
        Ok(stored) => {
            let text = stored.to_string();
            let _ = session.put(&key, text.clone()).await;
            drop(guard);
            let _ = query.reply(key, text).await;
        }
        Err(message) => {
            drop(guard);
            reply_config_err(&query, &message).await;
        }
    }
}

async fn reply_config_err(query: &zenoh::query::Query, message: &str) {
    let payload = serde_json::json!({ "error": message }).to_string();
    let _ = query.reply_err(payload).await;
}

/// Serves current health at `home/health/{unit}` to late joiners. The
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

/// The unit that publishes under `home/history/`: the house's recorder, of
/// which validation allows one (docs/design.md#reserved-classes).
fn recorder_unit(check: &CheckResult) -> Option<String> {
    check
        .expanded
        .iter()
        .find(|k| {
            k.direction == crate::expand::Direction::Publishes
                && k.source.starts_with("home/history/")
        })
        .map(|k| k.unit.clone())
}

/// Installs the SIGTERM/SIGINT handlers now and returns a future that waits
/// for either. A signal delivered between the two is held until the wait.
fn stop_signal() -> impl std::future::Future<Output = ()> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");
    async move {
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
    }
}
