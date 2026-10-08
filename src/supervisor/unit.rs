//! The per-unit supervision state machine. One task per unit owns its child
//! process, watches its liveliness token, applies the restart policy, and
//! publishes health at `home/health/{unit}`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use tokio::sync::watch;
use zenoh::sample::SampleKind;
use zenoh::Session;

use crate::bus::{self, Health, HealthStatus};
use crate::manifest::RestartPolicy;
use crate::supervisor::backoff::{Breaker, Decision};
use crate::supervisor::process;
use crate::supervisor::{HealthMap, LogMap};

const DEFAULT_GRACE_S: u32 = 5;

/// Everything a supervision task needs to run one unit.
pub struct UnitSpec {
    pub name: String,
    pub command: String,
    pub restart: RestartPolicy,
    pub grace: Duration,
    /// House repo root; the unit's cwd.
    pub cwd: PathBuf,
    /// Bus endpoint handed to the unit via `HOMEOSTAT_BUS`.
    pub endpoint: String,
    /// Environment variable names the manifest declares (`runtime.env`);
    /// passed through from the supervisor's environment, nothing else is.
    pub env: Vec<String>,
}

impl UnitSpec {
    /// The SIGTERM-to-SIGKILL grace from `shutdown_grace_s`; 5 s when unset.
    pub fn grace_from_manifest(grace_s: Option<u32>) -> Duration {
        Duration::from_secs(u64::from(grace_s.unwrap_or(DEFAULT_GRACE_S)))
    }

    /// The spec for a loaded unit, run from the house root against `endpoint`.
    pub fn from_loaded(
        unit: &crate::repo::LoadedUnit,
        root: &std::path::Path,
        endpoint: &str,
    ) -> UnitSpec {
        UnitSpec {
            name: unit.manifest.unit.name.clone(),
            command: unit.manifest.runtime.command.clone(),
            restart: unit.manifest.runtime.restart,
            grace: UnitSpec::grace_from_manifest(unit.manifest.runtime.shutdown_grace_s),
            cwd: root.to_path_buf(),
            endpoint: endpoint.to_string(),
            env: unit.manifest.runtime.env.clone().unwrap_or_default(),
        }
    }
}

/// Publishes health transitions and keeps the supervisor's shared health
/// map current; the map (served by a queryable) is what late joiners see,
/// so transitions are published exactly once.
struct HealthPublisher {
    session: Session,
    key: String,
    unit: String,
    map: HealthMap,
}

impl HealthPublisher {
    async fn set(&mut self, health: Health) {
        log_transition(&self.unit, &health);
        self.map
            .lock()
            .expect("health map lock")
            .insert(self.unit.clone(), health.clone());
        let payload = serde_json::to_string(&health).expect("health serializes");
        let _ = self.session.put(&self.key, payload).await;
    }
}

fn log_transition(unit: &str, health: &Health) {
    let status = match health.status {
        HealthStatus::Starting => "starting",
        HealthStatus::Running => "running",
        HealthStatus::Backoff => "backoff",
        HealthStatus::Open => "open",
        HealthStatus::Stopped => "stopped",
    };
    let mut line = format!("[homeostat] {unit}: {status}");
    if let Some(pid) = health.pid {
        line.push_str(&format!(" (pid {pid})"));
    }
    if let Some(ms) = health.backoff_ms {
        line.push_str(&format!(" (restart in {ms}ms)"));
    }
    if let Some(code) = health.last_exit_code {
        line.push_str(&format!(" (exit code {code})"));
    }
    println!("{line}");
}

enum RunOutcome {
    Exited(Option<i32>),
    Shutdown,
}

/// Supervises one unit until it stops for good or shutdown is signalled.
pub async fn supervise(
    spec: UnitSpec,
    session: Session,
    map: HealthMap,
    log: LogMap,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut health = HealthPublisher {
        session: session.clone(),
        key: bus::health_key(&spec.name),
        unit: spec.name.clone(),
        map,
    };

    let mut breaker = Breaker::new();
    let mut restarts: u32 = 0;

    loop {
        // A fresh subscriber per incarnation: reusing one across restarts
        // lets a queued Put from the previous incarnation mark the next one
        // running before its child even connected. history(true) still
        // catches a token declared between spawn and here.
        let token_sub = session
            .liveliness()
            .declare_subscriber(bus::liveliness_key(&spec.name))
            .history(true)
            .await
            .expect("liveliness subscriber");
        health
            .set(Health::idle(HealthStatus::Starting, restarts, None))
            .await;

        let env = [
            (bus::ENV_UNIT, spec.name.as_str()),
            (bus::ENV_BUS, spec.endpoint.as_str()),
        ];
        // Per incarnation, not once: a restart after an SDK bump must pick
        // up the new environment. See process::resolve.
        let command = process::resolve(&spec.command, &spec.cwd).await;
        let started = Instant::now();
        let mut child = match process::spawn(&command, &spec.cwd, &spec.env, &env) {
            Ok(child) => child,
            Err(err) => {
                eprintln!("[homeostat] {}: spawn failed: {err}", spec.name);
                let ran_for = started.elapsed();
                if back_off_or_open(
                    &mut health,
                    &mut breaker,
                    &mut restarts,
                    ran_for,
                    None,
                    &mut shutdown,
                )
                .await
                {
                    continue;
                }
                break;
            }
        };
        process::capture(&mut child, &spec.name, &log);
        let pid = child.id();

        let outcome = loop {
            tokio::select! {
                status = child.wait() => {
                    break RunOutcome::Exited(status.ok().and_then(|s| s.code()));
                }
                _ = shutdown.changed() => {
                    process::terminate(&mut child, spec.grace).await;
                    break RunOutcome::Shutdown;
                }
                sample = token_sub.recv_async() => {
                    if let Ok(sample) = sample {
                        // Put: the unit declared its token — running. Delete
                        // while the child is still alive: the token dropped
                        // (or a stale token from the previous incarnation
                        // just cleared) — back to starting until it returns.
                        let status = match sample.kind() {
                            SampleKind::Put => HealthStatus::Running,
                            SampleKind::Delete => HealthStatus::Starting,
                        };
                        health.set(Health {
                            pid,
                            ..Health::idle(status, restarts, None)
                        }).await;
                    }
                }
            }
        };

        // The group must not outlive its leader: sweep any descendants the
        // exited child left behind before deciding what happens next.
        if let (RunOutcome::Exited(_), Some(pid)) = (&outcome, pid) {
            process::sweep_group(pid);
        }

        let code = match outcome {
            RunOutcome::Shutdown => {
                health
                    .set(Health::idle(HealthStatus::Stopped, restarts, None))
                    .await;
                break;
            }
            RunOutcome::Exited(code) => code,
        };

        let done = match spec.restart {
            RestartPolicy::Never => true,
            RestartPolicy::OnFailure => code == Some(0),
            RestartPolicy::Always => false,
        };
        if done {
            health
                .set(Health::idle(HealthStatus::Stopped, restarts, code))
                .await;
            break;
        }

        let ran_for = started.elapsed();
        if !back_off_or_open(
            &mut health,
            &mut breaker,
            &mut restarts,
            ran_for,
            code,
            &mut shutdown,
        )
        .await
        {
            break;
        }
    }

    // A unit that stopped or opened its breaker keeps its last health state
    // visible through the supervisor's health queryable; nothing left to do.
}

/// The breaker's step after an incarnation ends with a restart due: open
/// the breaker, or count the restart, report the backoff and sleep it
/// out. False when the unit is done: the breaker opened, or shutdown
/// arrived during the backoff.
async fn back_off_or_open(
    health: &mut HealthPublisher,
    breaker: &mut Breaker,
    restarts: &mut u32,
    ran_for: Duration,
    code: Option<i32>,
    shutdown: &mut watch::Receiver<bool>,
) -> bool {
    match breaker.on_exit(ran_for) {
        Decision::Open => {
            health
                .set(Health::idle(HealthStatus::Open, *restarts, code))
                .await;
            false
        }
        Decision::Restart { delay } => {
            *restarts += 1;
            health
                .set(Health {
                    backoff_ms: Some(delay.as_millis() as u64),
                    ..Health::idle(HealthStatus::Backoff, *restarts, code)
                })
                .await;
            if backoff_interrupted(delay, shutdown).await {
                health
                    .set(Health::idle(HealthStatus::Stopped, *restarts, code))
                    .await;
                return false;
            }
            true
        }
    }
}

/// Sleeps for `delay`; returns true if shutdown arrived first.
async fn backoff_interrupted(delay: Duration, shutdown: &mut watch::Receiver<bool>) -> bool {
    tokio::select! {
        () = tokio::time::sleep(delay) => false,
        _ = shutdown.changed() => true,
    }
}
