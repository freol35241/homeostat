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
use crate::supervisor::HealthMap;

const DEFAULT_GRACE_S: u32 = 5;

pub struct UnitSpec {
    pub name: String,
    pub command: String,
    pub restart: RestartPolicy,
    pub grace: Duration,
    /// House repo root; the unit's cwd.
    pub cwd: PathBuf,
    /// Bus endpoint handed to the unit via HOMEOSTAT_BUS.
    pub endpoint: String,
}

impl UnitSpec {
    pub fn grace_from_manifest(grace_s: Option<u32>) -> Duration {
        Duration::from_secs(grace_s.unwrap_or(DEFAULT_GRACE_S) as u64)
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
    mut shutdown: watch::Receiver<bool>,
) {
    let mut health = HealthPublisher {
        session: session.clone(),
        key: bus::health_key(&spec.name),
        unit: spec.name.clone(),
        map,
    };
    let token_sub = session
        .liveliness()
        .declare_subscriber(bus::liveliness_key(&spec.name))
        .history(true)
        .await
        .expect("liveliness subscriber");

    let mut breaker = Breaker::new();
    let mut restarts: u32 = 0;

    loop {
        health
            .set(Health {
                status: HealthStatus::Starting,
                pid: None,
                restarts,
                backoff_ms: None,
                last_exit_code: None,
            })
            .await;

        let env = [
            (bus::ENV_UNIT, spec.name.as_str()),
            (bus::ENV_BUS, spec.endpoint.as_str()),
        ];
        let started = Instant::now();
        let mut child = match process::spawn(&spec.command, &spec.cwd, &env) {
            Ok(child) => child,
            Err(err) => {
                eprintln!("[homeostat] {}: spawn failed: {err}", spec.name);
                match breaker.on_exit(started.elapsed()) {
                    Decision::Open => {
                        health
                            .set(Health {
                                status: HealthStatus::Open,
                                pid: None,
                                restarts,
                                backoff_ms: None,
                                last_exit_code: None,
                            })
                            .await;
                        break;
                    }
                    Decision::Restart { delay } => {
                        restarts += 1;
                        if wait_backoff(&mut health, restarts, delay, &mut shutdown).await {
                            break;
                        }
                        continue;
                    }
                }
            }
        };
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
                        if sample.kind() == SampleKind::Put {
                            health.set(Health {
                                status: HealthStatus::Running,
                                pid,
                                restarts,
                                backoff_ms: None,
                                last_exit_code: None,
                            }).await;
                        }
                    }
                }
            }
        };

        let code = match outcome {
            RunOutcome::Shutdown => {
                health
                    .set(Health {
                        status: HealthStatus::Stopped,
                        pid: None,
                        restarts,
                        backoff_ms: None,
                        last_exit_code: None,
                    })
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
                .set(Health {
                    status: HealthStatus::Stopped,
                    pid: None,
                    restarts,
                    backoff_ms: None,
                    last_exit_code: code,
                })
                .await;
            break;
        }

        match breaker.on_exit(started.elapsed()) {
            Decision::Open => {
                health
                    .set(Health {
                        status: HealthStatus::Open,
                        pid: None,
                        restarts,
                        backoff_ms: None,
                        last_exit_code: code,
                    })
                    .await;
                break;
            }
            Decision::Restart { delay } => {
                restarts += 1;
                health
                    .set(Health {
                        status: HealthStatus::Backoff,
                        pid: None,
                        restarts,
                        backoff_ms: Some(delay.as_millis() as u64),
                        last_exit_code: code,
                    })
                    .await;
                if backoff_interrupted(delay, &mut shutdown).await {
                    health
                        .set(Health {
                            status: HealthStatus::Stopped,
                            pid: None,
                            restarts,
                            backoff_ms: None,
                            last_exit_code: code,
                        })
                        .await;
                    break;
                }
            }
        }
    }

    // A unit that stopped or opened its breaker keeps its last health state
    // visible through the supervisor's health queryable; nothing left to do.
}

/// Publishes backoff health and sleeps; returns true if shutdown arrived.
async fn wait_backoff(
    health: &mut HealthPublisher,
    restarts: u32,
    delay: Duration,
    shutdown: &mut watch::Receiver<bool>,
) -> bool {
    health
        .set(Health {
            status: HealthStatus::Backoff,
            pid: None,
            restarts,
            backoff_ms: Some(delay.as_millis() as u64),
            last_exit_code: None,
        })
        .await;
    backoff_interrupted(delay, shutdown).await
}

/// Sleeps for `delay`; returns true if shutdown arrived first.
async fn backoff_interrupted(delay: Duration, shutdown: &mut watch::Receiver<bool>) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(delay) => false,
        _ = shutdown.changed() => true,
    }
}
