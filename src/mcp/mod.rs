//! The agent surface (docs/design.md, "Agent surface (MCP)"): an MCP
//! server through which an agent observes the house. Read-only since
//! 2026-09-12: the write tools (`propose`, `apply`, `plan`) were removed
//! until a consumer exists — an agent with a filesystem edits the house
//! repo and runs the CLI, the same plan/apply path as every other actor.
//!
//! Six tools. `read_state` and `read_history` read the live bus (the
//! core's last-value caches, the recorder's history queryable); `read_logs`
//! and `read_events` read the operational exhaust and the durable audit
//! trail (docs/design.md, "Logs and the audit trail"); `schema` and
//! `explain` serve the authoring contract — the manifest schema and the
//! validator's rules — for an agent writing manifests through the repo.
//!
//! The server is a bus client like any observer: it needs no house root
//! and never touches the repo. Run under the supervisor as a service unit
//! it declares the unit liveliness token; standalone (stdio, launched by
//! an MCP client) it is just a CLI with a session.

pub mod http;
pub mod protocol;

use serde_json::{json, Value};
use zenoh::Session;

use crate::bus::{self, LogEntry};
use crate::world;

pub struct Server {
    session: Session,
    runtime: tokio::runtime::Runtime,
    /// Held for the process lifetime when running as a supervised unit.
    _liveliness: Option<zenoh::liveliness::LivelinessToken>,
}

impl Server {
    /// Connects to the live bus (an unreachable endpoint is a startup
    /// error, per the unit contract: supervisor backoff makes it visible),
    /// declares the liveliness token when running as a unit, and installs
    /// the SIGTERM/SIGINT handler.
    pub fn start(endpoint: &str) -> Result<Server, String> {
        let runtime = tokio::runtime::Runtime::new().map_err(|e| format!("tokio runtime: {e}"))?;
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
            session,
            runtime,
            _liveliness: liveliness,
        })
    }

    /// Handles one tools/call. Ok is the tool's text output; Err becomes an
    /// MCP tool result with isError set, not a protocol error.
    pub fn call(&self, name: &str, args: &Value) -> Result<String, String> {
        match name {
            "read_state" => self.read_state(args),
            "read_history" => self.read_history(args),
            "read_logs" => self.read_logs(args),
            "read_events" => self.read_events(args),
            "explain" => explain(args),
            "schema" => schema(args),
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
            let value = value
                .as_u64()
                .ok_or("\"limit\" must be a positive integer")?;
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
                        let rows: Value = serde_json::from_slice(&sample.payload().to_bytes())
                            .unwrap_or(Value::Null);
                        values.insert(sample.key_expr().to_string(), rows);
                    }
                    Err(err) => {
                        return Err(String::from_utf8_lossy(&err.payload().to_bytes()).to_string());
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
            let value = value
                .as_u64()
                .ok_or("\"lines\" must be a positive integer")?;
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
            let value = value
                .as_u64()
                .ok_or("\"limit\" must be a positive integer")?;
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
}

fn str_arg<'a>(args: &'a Value, name: &str) -> Result<&'a str, String> {
    args.get(name)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("missing \"{name}\" (string)"))
}

pub const TOOL_NAMES: &[&str] = &[
    "read_state",
    "read_history",
    "read_logs",
    "read_events",
    "explain",
    "schema",
];

/// The `schema` tool: the manifest contract as JSON Schema, one file kind
/// or all three — what an agent reads before authoring a unit, instead of
/// the validator's source.
fn schema(args: &Value) -> Result<String, String> {
    let value = match args.get("file").and_then(Value::as_str) {
        None => crate::schema::all(),
        Some(name) => match crate::schema::File::parse(name) {
            Some(file) => crate::schema::json(file),
            None => {
                return Err(format!(
                    "unknown file kind \"{name}\": expected unit, entity or zones"
                ))
            }
        },
    };
    Ok(serde_json::to_string_pretty(&value).expect("schema serializes"))
}

/// The `explain` tool: the registered paragraph for one error code, or
/// every code with its paragraph when none is given. Refused plans already
/// carry these inline; this is for an agent reading a code elsewhere (a
/// pending plan, a log) or surveying the contract before authoring.
fn explain(args: &Value) -> Result<String, String> {
    match args.get("code").and_then(Value::as_str) {
        Some(code) => crate::error::explain(code)
            .map(|text| format!("{code}: {text}"))
            .ok_or_else(|| format!("unknown error code \"{code}\"")),
        None => Ok(crate::error::CODES
            .iter()
            .map(|(code, text)| format!("{code}: {text}"))
            .collect::<Vec<_>>()
            .join("\n\n")),
    }
}

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
            "name": "schema",
            "description": "The manifest contract as JSON Schema, derived from the core's \
                own parser: every section and field of a unit manifest, an entity file \
                and zones.toml, with descriptions and which kinds accept what. Read it \
                before authoring a unit; pair it with explain for the validator's rules.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file": {"type": "string", "enum": ["unit", "entity", "zones"],
                             "description": "one file kind; omit for all three"}
                }
            }
        },
        {
            "name": "explain",
            "description": "Explain a validation error code from a refused plan \
                (the <code> in error[<code>]): the rule and why it exists. \
                Without a code, every code the core can emit, with its explanation \
                — the authoring contract's rules in one read.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "code": {"type": "string", "description": "an error code, e.g. state-publish-unbound"}
                }
            }
        }
    ])
}
