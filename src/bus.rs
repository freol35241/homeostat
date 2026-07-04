//! Zenoh session construction and the supervision key schema.
//!
//! Key schema (see docs/design.md):
//! - `home/health/{unit}`        supervisor-published JSON health status
//! - `home/health/{unit}/alive`  liveliness token declared by the unit itself
//! - `home/meta/{unit}/manifest_hash`  sha256 of the unit's manifest file
//! - `home/config/{unit}/{param}`  core-owned live parameter values
//!
//! All sessions run in peer mode with multicast scouting disabled; topology
//! is explicit (the supervisor listens, everyone else connects). This keeps
//! parallel test buses isolated and makes localhost deterministic.

use serde::{Deserialize, Serialize};
use zenoh::Config;

pub const DEFAULT_LISTEN: &str = "tcp/127.0.0.1:7447";

/// Environment variable carrying the unit's name into its process.
pub const ENV_UNIT: &str = "HOMEOSTAT_UNIT";
/// Environment variable carrying the bus connect endpoint into a unit.
pub const ENV_BUS: &str = "HOMEOSTAT_BUS";

pub fn health_key(unit: &str) -> String {
    format!("home/health/{unit}")
}

pub fn liveliness_key(unit: &str) -> String {
    format!("home/health/{unit}/alive")
}

pub fn manifest_hash_key(unit: &str) -> String {
    format!("home/meta/{unit}/manifest_hash")
}

pub fn config_key(unit: &str, param: &str) -> String {
    format!("home/config/{unit}/{param}")
}

/// Supervisor-side session config: a router listening on `endpoint`, no
/// scouting. Router mode makes the supervisor the hub that routes between
/// the units and any observer connected to it.
pub fn listen_config(endpoint: &str) -> Config {
    let mut config = base_config("router");
    config
        .insert_json5("listen/endpoints", &format!("[\"{endpoint}\"]"))
        .expect("valid listen endpoint config");
    config
}

/// Unit/observer-side session config: a client connected to `endpoint`.
pub fn connect_config(endpoint: &str) -> Config {
    let mut config = base_config("client");
    config
        .insert_json5("connect/endpoints", &format!("[\"{endpoint}\"]"))
        .expect("valid connect endpoint config");
    config
}

fn base_config(mode: &str) -> Config {
    let mut config = Config::default();
    config
        .insert_json5("mode", &format!("\"{mode}\""))
        .expect("valid mode config");
    config
        .insert_json5("scouting/multicast/enabled", "false")
        .expect("valid scouting config");
    config
        .insert_json5("scouting/gossip/enabled", "false")
        .expect("valid scouting config");
    config
}

/// Supervision status of a unit, published as JSON at `home/health/{unit}`
/// on every transition and refreshed periodically for late subscribers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HealthStatus {
    /// Process spawned, liveliness token not yet seen.
    Starting,
    /// Liveliness token present on the bus.
    Running,
    /// Process exited; restart scheduled after `backoff_ms`.
    Backoff,
    /// Circuit breaker open: too many consecutive quick failures, no
    /// further restarts until the supervisor itself is restarted.
    Open,
    /// Not running and not coming back (policy `never`, clean exit under
    /// `on-failure`, or supervisor shutdown).
    Stopped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Health {
    pub status: HealthStatus,
    pub pid: Option<u32>,
    /// Restarts performed since the supervisor started this unit.
    pub restarts: u32,
    /// Planned restart delay; only present while `status == "backoff"`.
    pub backoff_ms: Option<u64>,
    /// Exit code of the most recent exit, if it exited with one.
    pub last_exit_code: Option<i32>,
}
