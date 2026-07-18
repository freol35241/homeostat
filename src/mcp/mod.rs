//! The agent surface (docs/design.md, step 6): an MCP server through which
//! an agent observes the house and changes it, with authority bounded by
//! the same plan/apply machinery as every other actor.
//!
//! Seven tools. `read_state` and `read_history` read the live bus (the
//! core's last-value caches, the recorder's history queryable); `read_logs`
//! and `read_events` read the operational exhaust and the durable audit
//! trail (docs/design.md, "Logs and the audit trail"). `propose`
//! takes text — house-repo paths plus full new content — commits it to the
//! current branch, and plans: a parameter-only plan auto-applies (zero
//! restarts, durable by construction); anything behavioral or structural
//! is saved as a pending plan for the owner. `apply` executes the current
//! diff only when it is parameter-only — the tier gates the actor, and the
//! tier is derived mechanically, so a grant delta can never be smuggled
//! through as a parameter edit.
//!
//! The server is a bus client like any observer. Run under the supervisor
//! as a service unit it declares the unit liveliness token; standalone
//! (stdio, launched by an MCP client) it is just a CLI with a session.

pub mod http;
pub mod protocol;

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use serde_json::{json, Value};
use zenoh::Session;

use crate::bus::{self, ApplyRequest, ApplyResult, LogEntry};
use crate::plan::{self, Tier};
use crate::{gitinfo, pending, world};

/// The actor tier recorded in agent-authored pending plans.
pub const ACTOR: &str = "agent";

pub struct Server {
    root: PathBuf,
    endpoint: String,
    session: Session,
    runtime: tokio::runtime::Runtime,
    /// Held for the process lifetime when running as a supervised unit.
    _liveliness: Option<zenoh::liveliness::LivelinessToken>,
    /// Serializes tool calls: propose and apply mutate the repo and the
    /// world, and agent traffic never needs concurrency.
    lock: Mutex<()>,
}

impl Server {
    /// Connects to the live bus (an unreachable endpoint is a startup
    /// error, per the unit contract: supervisor backoff makes it visible),
    /// declares the liveliness token when running as a unit, and installs
    /// the SIGTERM/SIGINT handler.
    pub fn start(root: &Path, endpoint: &str) -> Result<Server, String> {
        let runtime = tokio::runtime::Runtime::new()
            .map_err(|e| format!("tokio runtime: {e}"))?;
        let session = runtime.block_on(world::connect(endpoint))?;
        let liveliness = match std::env::var(bus::ENV_UNIT) {
            Ok(unit) if !unit.is_empty() => Some(
                runtime
                    .block_on(async {
                        session
                            .liveliness()
                            .declare_token(bus::liveliness_key(&unit))
                            .await
                    })
                    .map_err(|e| format!("cannot declare liveliness token: {e}"))?,
            ),
            _ => None,
        };
        runtime.spawn(async {
            use tokio::signal::unix::{signal, SignalKind};
            let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
            let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");
            tokio::select! {
                _ = term.recv() => {}
                _ = int.recv() => {}
            }
            std::process::exit(0);
        });
        Ok(Server {
            root: root.to_path_buf(),
            endpoint: endpoint.to_string(),
            session,
            runtime,
            _liveliness: liveliness,
            lock: Mutex::new(()),
        })
    }

    /// Handles one tools/call. Ok is the tool's text output; Err becomes an
    /// MCP tool result with isError set, not a protocol error.
    pub fn call(&self, name: &str, args: &Value) -> Result<String, String> {
        let _guard = self.lock.lock().expect("mcp tool lock");
        match name {
            "read_state" => self.read_state(args),
            "read_history" => self.read_history(args),
            "read_logs" => self.read_logs(args),
            "read_events" => self.read_events(args),
            "plan" => self.plan(),
            "propose" => self.propose(args),
            "apply" => self.apply(),
            _ => Err(format!("unknown tool \"{name}\"")),
        }
    }

    fn read_state(&self, args: &Value) -> Result<String, String> {
        let key = str_arg(args, "key")?;
        if !(key == "home" || key.starts_with("home/")) {
            return Err("read_state reads the house bus: the key must be under home/".into());
        }
        let values = self.runtime.block_on(async {
            let replies = self
                .session
                .get(key)
                .await
                .map_err(|e| format!("bus read failed: {e}"))?;
            let mut values = serde_json::Map::new();
            while let Ok(reply) = replies.recv_async().await {
                if let Ok(sample) = reply.result() {
                    let bytes = sample.payload().to_bytes();
                    let value = serde_json::from_slice(&bytes).unwrap_or_else(|_| {
                        Value::String(String::from_utf8_lossy(&bytes).to_string())
                    });
                    values.insert(sample.key_expr().to_string(), value);
                }
            }
            Ok::<_, String>(values)
        })?;
        serde_json::to_string_pretty(&Value::Object(values))
            .map_err(|e| format!("values do not serialize: {e}"))
    }

    fn read_history(&self, args: &Value) -> Result<String, String> {
        let series = str_arg(args, "series")?;
        if series.starts_with("home/") || series.contains('?') {
            return Err(
                "series is relative to home/history/ — {state|cmd}/{entity}/{aspect}, \
                 e.g. state/livingroom_lamp/on"
                    .into(),
            );
        }
        let mut params = Vec::new();
        for name in ["from", "to"] {
            if let Some(value) = args.get(name) {
                let value = value
                    .as_str()
                    .ok_or(format!("\"{name}\" must be an RFC3339 string"))?;
                params.push(format!("{name}={value}"));
            }
        }
        if let Some(value) = args.get("limit") {
            let value = value.as_u64().ok_or("\"limit\" must be a positive integer")?;
            params.push(format!("limit={value}"));
        }
        let selector = if params.is_empty() {
            format!("home/history/{series}")
        } else {
            format!("home/history/{series}?{}", params.join(";"))
        };

        self.runtime.block_on(async {
            let replies = self
                .session
                .get(&selector)
                .await
                .map_err(|e| format!("history read failed: {e}"))?;
            let mut values = serde_json::Map::new();
            while let Ok(reply) = replies.recv_async().await {
                match reply.result() {
                    Ok(sample) => {
                        let rows: Value =
                            serde_json::from_slice(&sample.payload().to_bytes())
                                .unwrap_or(Value::Null);
                        values.insert(sample.key_expr().to_string(), rows);
                    }
                    Err(err) => {
                        return Err(String::from_utf8_lossy(&err.payload().to_bytes())
                            .to_string());
                    }
                }
            }
            serde_json::to_string_pretty(&Value::Object(values))
                .map_err(|e| format!("rows do not serialize: {e}"))
        })
    }

    /// Reads a unit's captured stdout/stderr ring buffer over the bus and
    /// renders it as one "ts_us stream line" row per captured line —
    /// operational exhaust for debugging, gone on supervisor restart.
    fn read_logs(&self, args: &Value) -> Result<String, String> {
        let unit = str_arg(args, "unit")?;
        let mut selector = bus::log_key(unit);
        if let Some(value) = args.get("lines") {
            let value = value.as_u64().ok_or("\"lines\" must be a positive integer")?;
            selector.push_str(&format!("?lines={value}"));
        }
        self.runtime.block_on(async {
            let replies = self
                .session
                .get(&selector)
                .await
                .map_err(|e| format!("log read failed: {e}"))?;
            let mut rows = Vec::new();
            while let Ok(reply) = replies.recv_async().await {
                match reply.result() {
                    Ok(sample) => {
                        let entries: Vec<LogEntry> =
                            serde_json::from_slice(&sample.payload().to_bytes())
                                .map_err(|e| format!("log entries do not parse: {e}"))?;
                        for entry in entries {
                            rows.push(format!("{} {} {}", entry.ts_us, entry.stream, entry.line));
                        }
                    }
                    Err(err) => {
                        return Err(String::from_utf8_lossy(&err.payload().to_bytes()).to_string());
                    }
                }
            }
            Ok(rows.join("\n"))
        })
    }

    /// Reads the recorder's durable events trail over the bus: health
    /// events, preemptions, config writes, and cmd envelopes with their
    /// actors. Rows are {ts, key, payload}, returned as JSON text.
    fn read_events(&self, args: &Value) -> Result<String, String> {
        let mut params = Vec::new();
        if let Some(value) = args.get("key") {
            let value = value.as_str().ok_or("\"key\" must be a string")?;
            params.push(format!("key={value}"));
        }
        for name in ["from", "to"] {
            if let Some(value) = args.get(name) {
                // Integer µs UTC, the recorder's native convention and the
                // same unit the reply's ts carries — not read_history's
                // RFC3339; an events range refines directly from prior rows.
                let value = value
                    .as_i64()
                    .ok_or(format!("\"{name}\" must be an integer (microseconds UTC)"))?;
                params.push(format!("{name}={value}"));
            }
        }
        if let Some(value) = args.get("limit") {
            let value = value.as_u64().ok_or("\"limit\" must be a positive integer")?;
            params.push(format!("limit={value}"));
        }
        let selector = if params.is_empty() {
            "home/history/events".to_string()
        } else {
            format!("home/history/events?{}", params.join(";"))
        };

        self.runtime.block_on(async {
            let replies = self
                .session
                .get(&selector)
                .await
                .map_err(|e| format!("events read failed: {e}"))?;
            let mut rows = Vec::new();
            while let Ok(reply) = replies.recv_async().await {
                match reply.result() {
                    Ok(sample) => {
                        let payload = sample.payload().to_bytes();
                        if let Ok(Value::Array(mut entries)) = serde_json::from_slice(&payload) {
                            rows.append(&mut entries);
                        }
                    }
                    Err(err) => {
                        return Err(String::from_utf8_lossy(&err.payload().to_bytes()).to_string());
                    }
                }
            }
            serde_json::to_string_pretty(&Value::Array(rows))
                .map_err(|e| format!("rows do not serialize: {e}"))
        })
    }

    fn plan(&self) -> Result<String, String> {
        let (check, world) = self.checked_world()?;
        Ok(self.render(&check, &world))
    }

    /// The tier gates the actor: parameter-only applies immediately;
    /// behavioral and structural are the owner's to apply from a pending
    /// plan (propose saves one).
    fn apply(&self) -> Result<String, String> {
        let (check, world) = self.checked_world()?;
        let diff = plan::diff(&check, &self.root, &world);
        let text = self.render(&check, &world);
        if diff.is_empty() {
            return Ok(text);
        }
        if plan::derive_tier(&diff) != Tier::ParameterOnly {
            return Err(format!(
                "apply refused at agent tier: the plan is {}. Owner approval required — \
                 propose saves a pending plan the owner applies with \
                 `homeostat apply --plan`.\n\n{text}",
                plan::derive_tier(&diff)
            ));
        }
        let outcome = self.send_apply()?;
        Ok(format!("{text}\n{outcome}"))
    }

    fn propose(&self, args: &Value) -> Result<String, String> {
        let message = str_arg(args, "message")?;
        let files = args
            .get("files")
            .and_then(Value::as_array)
            .filter(|f| !f.is_empty())
            .ok_or("propose needs a non-empty \"files\" array")?;
        if gitinfo::head_commit(&self.root).is_none() {
            return Err(format!(
                "propose needs the house root to be a git worktree root with a commit: {}",
                self.root.display()
            ));
        }

        let mut edits: Vec<(String, String)> = Vec::new();
        for file in files {
            let path = file
                .get("path")
                .and_then(Value::as_str)
                .ok_or("every file needs a \"path\" (string)")?;
            let content = file
                .get("content")
                .and_then(Value::as_str)
                .ok_or("every file needs a \"content\" (string)")?;
            check_repo_path(path)?;
            edits.push((path.to_string(), content.to_string()));
        }

        // Write, validate, and on a validation failure restore every file:
        // an invalid repo is never committed, the working tree stays clean.
        let mut originals: Vec<(String, Option<Vec<u8>>)> = Vec::new();
        for (path, content) in &edits {
            let full = self.root.join(path);
            originals.push((path.clone(), fs::read(&full).ok()));
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
            }
            fs::write(&full, content)
                .map_err(|e| format!("cannot write {}: {e}", full.display()))?;
        }
        let check = crate::check(&self.root);
        if !check.errors.is_empty() {
            self.restore(&originals);
            return Err(format!(
                "propose reverted: the repo would fail validation\n{}",
                crate::error::render_sorted(&check.errors).join("\n")
            ));
        }

        for (path, _) in &edits {
            git(&self.root, &["add", "--", path])?;
        }
        git(
            &self.root,
            &[
                "-c",
                "user.name=homeostat-agent",
                "-c",
                "user.email=agent@homeostat.local",
                "commit",
                "-q",
                "-m",
                message,
            ],
        )?;
        let head = gitinfo::head_commit(&self.root)
            .ok_or("the commit landed but HEAD is unreadable")?;

        let world = self
            .runtime
            .block_on(world::read(&self.session, &self.endpoint))?;
        let diff = plan::diff(&check, &self.root, &world);
        let text = self.render(&check, &world);
        if diff.is_empty() {
            return Ok(format!("Committed {head}.\n\n{text}"));
        }
        let tier = plan::derive_tier(&diff);
        if tier == Tier::ParameterOnly {
            return match self.send_apply() {
                Ok(outcome) => Ok(format!("Committed {head}.\n\n{text}\n{outcome}")),
                Err(err) => Err(format!(
                    "Committed {head}; the plan is parameter-only but apply failed: {err}"
                )),
            };
        }
        let saved = pending::save(&self.root, &text, &tier.to_string(), ACTOR, &head)?;
        Ok(format!(
            "Committed {head}.\n\n{text}\nPending plan saved: {saved}\n\
             The plan is {tier}: owner approval required — the owner applies it with \
             `homeostat apply --plan {saved}`.",
            saved = saved.display()
        ))
    }

    fn checked_world(&self) -> Result<(crate::CheckResult, plan::World), String> {
        let check = crate::check(&self.root);
        if !check.errors.is_empty() {
            return Err(format!(
                "the house repo fails validation\n{}",
                crate::error::render_sorted(&check.errors).join("\n")
            ));
        }
        let world = self
            .runtime
            .block_on(world::read(&self.session, &self.endpoint))?;
        Ok((check, world))
    }

    fn render(&self, check: &crate::CheckResult, world: &plan::World) -> String {
        plan::render(check, &self.root, &self.root.display().to_string(), world)
    }

    fn send_apply(&self) -> Result<String, String> {
        let request = ApplyRequest {
            base_commit: gitinfo::head_commit(&self.root),
        };
        let (outcome, replied_ok) = self
            .runtime
            .block_on(bus::request_apply(&self.session, &request))?;
        outcome_text(&outcome, replied_ok)
    }

    fn restore(&self, originals: &[(String, Option<Vec<u8>>)]) {
        for (path, original) in originals {
            let full = self.root.join(path);
            match original {
                Some(bytes) => {
                    let _ = fs::write(&full, bytes);
                }
                None => {
                    let _ = fs::remove_file(&full);
                }
            }
        }
    }
}

/// A proposed path must stay inside the house repo and out of the spaces
/// the machinery owns (git internals, plan artifacts).
fn check_repo_path(path: &str) -> Result<(), String> {
    let ok = !path.is_empty()
        && Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
        && !path.starts_with(".git/")
        && !path.starts_with("plans/");
    if ok {
        Ok(())
    } else {
        Err(format!(
            "\"{path}\" is not a plain repo-relative path (no absolute paths, no .., \
             not under .git/ or plans/)"
        ))
    }
}

fn outcome_text(outcome: &ApplyResult, replied_ok: bool) -> Result<String, String> {
    if let Some(error) = &outcome.error {
        return Err(format!("apply refused: {error}"));
    }
    let mut out = String::new();
    for param in &outcome.params {
        out.push_str(&format!(
            "  parameter {}/{} = {}\n",
            param.unit, param.param, param.value
        ));
    }
    for step in &outcome.steps {
        match &step.error {
            None => out.push_str(&format!("  {} {}: ok\n", step.action, step.unit)),
            Some(error) => out.push_str(&format!(
                "  {} {}: FAILED ({error})\n",
                step.action, step.unit
            )),
        }
    }
    if outcome.ok && replied_ok {
        out.push_str("Applied.\n");
        Ok(out)
    } else {
        Err(format!(
            "{out}apply halted at {}; not reached: {}",
            outcome.halted_at.as_deref().unwrap_or("?"),
            if outcome.not_reached.is_empty() {
                "none".to_string()
            } else {
                outcome.not_reached.join(", ")
            },
        ))
    }
}

fn str_arg<'a>(args: &'a Value, name: &str) -> Result<&'a str, String> {
    args.get(name)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("missing \"{name}\" (string)"))
}

fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub const TOOL_NAMES: &[&str] = &[
    "read_state",
    "read_history",
    "read_logs",
    "read_events",
    "plan",
    "propose",
    "apply",
];

/// The tool list served by tools/list.
pub fn tools() -> Value {
    json!([
        {
            "name": "read_state",
            "description": "Read live values from the house bus by key expression: \
                state, health, config, clock, meta, discovery. Wildcards allowed, \
                e.g. home/state/** or home/discovery/zigbee.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": {"type": "string", "description": "a home/** key expression"}
                },
                "required": ["key"]
            }
        },
        {
            "name": "read_history",
            "description": "Read recorded history over the bus. A series is \
                {state|cmd}/{entity}/{aspect} (wildcards allowed); rows are \
                {ts, room, value}, ascending, one reply per concrete series.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "series": {"type": "string", "description": "e.g. state/livingroom_lamp/on"},
                    "from": {"type": "string", "description": "RFC3339 with offset"},
                    "to": {"type": "string", "description": "RFC3339 with offset"},
                    "limit": {"type": "integer", "description": "keep the most recent rows"}
                },
                "required": ["series"]
            }
        },
        {
            "name": "read_logs",
            "description": "Read a unit's captured stdout/stderr: the last 500 lines, \
                operational exhaust for debugging, gone on supervisor restart (not the \
                durable trail — see read_events). One \"ts_us stream line\" row per line.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "unit": {"type": "string", "description": "unit name"},
                    "lines": {"type": "integer", "description": "keep only the last N lines"}
                },
                "required": ["unit"]
            }
        },
        {
            "name": "read_events",
            "description": "Read the durable audit trail: health events, preemptions, \
                config writes, and cmd envelopes with their actors. Rows are \
                {ts, key, payload}, ascending, as a JSON array.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": {"type": "string", "description": "a home/** key expression, wildcards allowed"},
                    "from": {"type": "integer", "description": "microseconds UTC, same unit as the reply ts"},
                    "to": {"type": "integer", "description": "microseconds UTC, same unit as the reply ts"},
                    "limit": {"type": "integer", "description": "keep the most recent rows"}
                }
            }
        },
        {
            "name": "plan",
            "description": "Diff the house repo against the live world; returns the \
                rendered plan and its tier (parameter-only, behavioral, structural).",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "propose",
            "description": "Write file changes into the house repo, commit them to the \
                current branch, and plan. A parameter-only plan applies immediately \
                (zero restarts); a behavioral or structural plan is saved as a pending \
                plan for the owner. Invalid changes are reverted, never committed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "files": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": {"type": "string", "description": "repo-relative path"},
                                "content": {"type": "string", "description": "full new file content"}
                            },
                            "required": ["path", "content"]
                        }
                    },
                    "message": {"type": "string", "description": "commit message stating the intent"}
                },
                "required": ["files", "message"]
            }
        },
        {
            "name": "apply",
            "description": "Apply the current diff if it is parameter-only. Behavioral \
                and structural plans are refused at agent tier: the owner applies those \
                from a pending plan.",
            "inputSchema": {"type": "object", "properties": {}}
        }
    ])
}
