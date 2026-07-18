//! Zenoh session construction and the supervision key schema.
//!
//! Key schema (see docs/design.md):
//! - `home/health/{unit}`        supervisor-published JSON health status
//! - `home/health/{unit}/alive`  liveliness token declared by the unit itself
//! - `home/meta/{unit}/manifest_hash`  sha256 of the unit's manifest file
//! - `home/meta/{unit}/log`  ring buffer (500 lines) of captured stdout/stderr
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

pub fn files_hash_key(unit: &str) -> String {
    format!("home/meta/{unit}/files_hash")
}

pub fn manifest_key(unit: &str) -> String {
    format!("home/meta/{unit}/manifest")
}

pub fn log_key(unit: &str) -> String {
    format!("home/meta/{unit}/log")
}

pub const GRANTS_KEY: &str = "home/meta/system/grants";
pub const APPLIED_COMMIT_KEY: &str = "home/meta/system/applied_commit";
/// The apply control queryable: a GET with payload is an apply request
/// (the same query-as-command pattern as config writes).
pub const APPLY_KEY: &str = "home/meta/system/apply";

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

/// One captured line from a unit's stdout or stderr, held in the per-unit
/// ring buffer and served as JSON at `home/meta/{unit}/log`. Operational
/// exhaust, not the durable trail: bounded memory, gone on supervisor
/// restart, never recorded.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    /// Capture time, microseconds since the Unix epoch, UTC — the same
    /// convention the recorder uses for its own timestamps.
    pub ts_us: i64,
    /// "stdout" | "stderr"
    pub stream: String,
    pub line: String,
}

/// Payload of a GET on `home/meta/system/apply`: the apply request the CLI
/// sends to the running supervisor (see docs/design.md, step 5b).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyRequest {
    /// The repo's HEAD when the house root is a git worktree root;
    /// published at `home/meta/system/applied_commit` on success.
    pub base_commit: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyParam {
    pub unit: String,
    pub param: String,
    pub value: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyStep {
    pub unit: String,
    /// "stop" | "start" | "restart"
    pub action: String,
    pub ok: bool,
    pub error: Option<String>,
}

/// Sends an apply request to the running supervisor and decodes its reply.
/// The bool is false when the supervisor replied with an error reply (a
/// refused or halted apply); the ApplyResult carries the detail either way.
pub async fn request_apply(
    session: &zenoh::Session,
    request: &ApplyRequest,
) -> Result<(ApplyResult, bool), String> {
    let replies = session
        .get(APPLY_KEY)
        .payload(serde_json::to_string(request).expect("request serializes"))
        .timeout(std::time::Duration::from_secs(600))
        .await
        .map_err(|e| e.to_string())?;
    let Ok(reply) = replies.recv_async().await else {
        return Err("no reply from the supervisor".to_string());
    };
    let (payload, replied_ok) = match reply.result() {
        Ok(sample) => (sample.payload().to_bytes().to_vec(), true),
        Err(err) => (err.payload().to_bytes().to_vec(), false),
    };
    match serde_json::from_slice::<ApplyResult>(&payload) {
        Ok(outcome) => Ok((outcome, replied_ok)),
        Err(_) => Err(String::from_utf8_lossy(&payload).to_string()),
    }
}

/// The supervisor's reply to an apply request. `steps` holds every unit
/// step attempted, in walk order; a halted walk names its position.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyResult {
    pub ok: bool,
    pub tier: Option<String>,
    pub params: Vec<ApplyParam>,
    pub steps: Vec<ApplyStep>,
    /// Unit at which the walk halted in place, if it did.
    pub halted_at: Option<String>,
    /// Units the walk never reached.
    pub not_reached: Vec<String>,
    pub error: Option<String>,
}
