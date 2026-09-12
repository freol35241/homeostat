//! Minimal JSON-RPC / MCP request handling shared by both transports:
//! initialize, tools/list, tools/call, ping. Hand-rolled on purpose — the
//! surface this server needs is five methods over JSON-RPC 2.0, stateless,
//! one message at a time, and stays that way; an SDK would be the largest
//! dependency in the tree.

use serde_json::{json, Value};

use super::Server;

/// The newest MCP revision this server knows; initialize echoes the
/// client's requested revision since everything served here is valid under
/// every published one.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// Handles one JSON-RPC message; None for notifications (no reply).
pub fn handle(server: &Server, message: &Value) -> Option<Value> {
    let id = match message.get("id") {
        Some(id) if !id.is_null() => id.clone(),
        _ => return None,
    };
    let method = message
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let params = message.get("params").cloned().unwrap_or_else(|| json!({}));
    let result = match method {
        "initialize" => Ok(initialize(&params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": super::tools()})),
        "tools/call" => tool_call(server, &params),
        _ => Err(json!({
            "code": -32601,
            "message": format!("method \"{method}\" not found")
        })),
    };
    Some(match result {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(error) => json!({"jsonrpc": "2.0", "id": id, "error": error}),
    })
}

fn initialize(params: &Value) -> Value {
    let version = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(PROTOCOL_VERSION);
    json!({
        "protocolVersion": version,
        "capabilities": {"tools": {}},
        "serverInfo": {"name": "homeostat", "version": env!("CARGO_PKG_VERSION")}
    })
}

/// An unknown tool is a protocol error; a known tool that fails is a tool
/// result with isError — the agent reads the message and adjusts.
fn tool_call(server: &Server, params: &Value) -> Result<Value, Value> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !super::TOOL_NAMES.contains(&name) {
        return Err(json!({
            "code": -32602,
            "message": format!("unknown tool \"{name}\"")
        }));
    }
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let (text, is_error) = match server.call(name, &args) {
        Ok(text) => (text, false),
        Err(text) => (text, true),
    };
    Ok(json!({
        "content": [{"type": "text", "text": text}],
        "isError": is_error
    }))
}

/// The longest stdio message accepted: generous for any tool call, and a
/// bound rather than an allocation failure for a runaway client.
const MAX_LINE: u64 = 16 * 1024 * 1024;

/// The stdio transport: newline-delimited JSON-RPC on stdin/stdout, one
/// response line per request. Returns on EOF — the MCP client hanging up
/// is the shutdown signal.
pub fn serve_stdio(server: &Server) -> Result<(), String> {
    use std::io::{BufRead, Read, Write};
    let mut stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout();
    loop {
        let mut line = String::new();
        let n = (&mut stdin)
            .take(MAX_LINE + 1)
            .read_line(&mut line)
            .map_err(|e| format!("stdin: {e}"))?;
        if n == 0 {
            break;
        }
        let reply = if line.len() as u64 > MAX_LINE {
            // Skip the rest of the oversize message so the next line
            // starts clean, then answer with an error instead of aborting.
            let mut rest = Vec::new();
            while !line.ends_with('\n') && !rest.ends_with(b"\n") {
                rest.clear();
                let n = (&mut stdin)
                    .take(64 * 1024)
                    .read_until(b'\n', &mut rest)
                    .map_err(|e| format!("stdin: {e}"))?;
                if n == 0 {
                    break;
                }
            }
            Some(json!({
                "jsonrpc": "2.0",
                "id": null,
                "error": {"code": -32600, "message": format!("message exceeds {MAX_LINE} bytes")}
            }))
        } else if line.trim().is_empty() {
            continue;
        } else {
            match serde_json::from_str::<Value>(&line) {
                Ok(message) => handle(server, &message),
                Err(err) => Some(json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": {"code": -32700, "message": format!("parse error: {err}")}
                })),
            }
        };
        if let Some(reply) = reply {
            let text = serde_json::to_string(&reply).expect("reply serializes");
            writeln!(stdout, "{text}").map_err(|e| format!("stdout: {e}"))?;
            stdout.flush().map_err(|e| format!("stdout: {e}"))?;
        }
    }
    Ok(())
}
