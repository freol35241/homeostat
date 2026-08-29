//! The HTTP transport: MCP streamable-HTTP in its stateless shape. A POST
//! carries one JSON-RPC message and gets the JSON response in the body
//! (the spec allows `application/json` in place of an SSE stream); a
//! notification gets 202 with no body; GET is 405 — this server never
//! initiates messages, so there is no stream to open and no session to
//! manage. Thread per connection: agent traffic is a conversation, not a
//! load profile.
//!
//! Reachability is the credential (docs/design.md, Local-only access), and
//! the design draws the consequence: the browser is NOT local even when the
//! house is, so a public page in a family browser can fire requests at LAN
//! addresses. This surface writes and commits to the house repo, so it
//! carries the same three gates the dashboard has had from day one —
//! `Host` must be non-global or known, `Origin` must be absent (no browser
//! sent it) or allowed, and every request must carry `X-Homeostat`. That
//! header is what a cross-origin `fetch` cannot add without a preflight
//! the 405 on OPTIONS refuses; without it a `text/plain` POST is a CORS
//! "simple request" and lands as a blind write.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

use serde_json::{json, Value};

use super::{protocol, Server};

/// Mirrors the dashboard's list (adapters/dashboard.py).
const ALLOWED_NAMES: [&str; 4] = ["localhost", "homeostat", "homeostat.lan", "homeostat.local"];
const WRITE_HEADER: &str = "x-homeostat";
const ENV_HOSTS: &str = "HOMEOSTAT_MCP_HOSTS";

/// The host part of a `Host` or `Origin` header value: strips a scheme and
/// a port, and unwraps a bracketed IPv6 literal.
fn host_of(value: &str) -> &str {
    let value = value.rsplit_once("://").map_or(value, |(_, rest)| rest);
    if let Some(rest) = value.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest);
    }
    match value.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) => host,
        _ => value,
    }
}

/// A non-global address, or a name the house answers to. A rebound public
/// domain arrives as its own name and is refused.
fn host_allowed(value: &str) -> bool {
    let host = host_of(value);
    if ALLOWED_NAMES.contains(&host) {
        return true;
    }
    if std::env::var(ENV_HOSTS).is_ok_and(|extra| extra.split(',').any(|h| h.trim() == host)) {
        return true;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(ip) => !ip_is_global(&ip),
        Err(_) => false,
    }
}

/// `IpAddr::is_global` is unstable, and the question here is only whether
/// the peer is plausibly on the house's own network.
fn ip_is_global(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            !(v4.is_private() || v4.is_loopback() || v4.is_link_local() || v4.is_unspecified())
        }
        std::net::IpAddr::V6(v6) => {
            let seg = v6.segments()[0];
            !(v6.is_loopback() || v6.is_unspecified() || seg & 0xfe00 == 0xfc00 || seg & 0xffc0 == 0xfe80)
        }
    }
}

pub fn serve(server: Arc<Server>, addr: &str) -> Result<(), String> {
    let listener =
        TcpListener::bind(addr).map_err(|e| format!("cannot listen on {addr}: {e}"))?;
    eprintln!("[homeostat] mcp listening on http://{addr}");
    loop {
        let (stream, _) = match listener.accept() {
            Ok(accepted) => accepted,
            Err(_) => continue,
        };
        let server = server.clone();
        std::thread::spawn(move || {
            let _ = connection(&server, stream);
        });
    }
}

/// Serves requests on one connection until the peer hangs up or asks to
/// close (keep-alive is HTTP/1.1's default and real MCP clients use it).
fn connection(server: &Server, stream: TcpStream) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut stream = stream;
    loop {
        let mut request_line = String::new();
        if reader.read_line(&mut request_line)? == 0 {
            return Ok(());
        }
        let method = request_line.split_whitespace().next().unwrap_or("").to_string();

        let mut content_length = 0usize;
        let mut close = false;
        let mut host: Option<String> = None;
        let mut origin: Option<String> = None;
        let mut write_header = false;
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header)? == 0 {
                return Ok(());
            }
            let header = header.trim_end();
            if header.is_empty() {
                break;
            }
            let Some((name, value)) = header.split_once(':') else { continue };
            let value = value.trim();
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.parse().unwrap_or(0);
            } else if name.eq_ignore_ascii_case("connection")
                && value.eq_ignore_ascii_case("close")
            {
                close = true;
            } else if name.eq_ignore_ascii_case("host") {
                host = Some(value.to_string());
            } else if name.eq_ignore_ascii_case("origin") {
                origin = Some(value.to_string());
            } else if name.eq_ignore_ascii_case(WRITE_HEADER) {
                write_header = true;
            }
        }
        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body)?;

        // Refused before the body is parsed: a rejected request must not
        // reach a tool, and the reason is never echoed to a browser.
        if let Some(refusal) = gate(host.as_deref(), origin.as_deref(), write_header) {
            respond(&mut stream, "403 Forbidden", &[], refusal.as_bytes())?;
        } else if method != "POST" {
            respond(&mut stream, "405 Method Not Allowed", &[("Allow", "POST")], b"")?;
        } else {
            match serde_json::from_slice::<Value>(&body) {
                Ok(message) => match protocol::handle(server, &message) {
                    Some(reply) => {
                        let body = serde_json::to_vec(&reply).expect("reply serializes");
                        respond(
                            &mut stream,
                            "200 OK",
                            &[("Content-Type", "application/json")],
                            &body,
                        )?;
                    }
                    None => respond(&mut stream, "202 Accepted", &[], b"")?,
                },
                Err(err) => {
                    let error = json!({
                        "jsonrpc": "2.0",
                        "id": null,
                        "error": {"code": -32700, "message": format!("parse error: {err}")}
                    });
                    respond(
                        &mut stream,
                        "400 Bad Request",
                        &[("Content-Type", "application/json")],
                        &serde_json::to_vec(&error).expect("error serializes"),
                    )?;
                }
            }
        }
        if close {
            return Ok(());
        }
    }
}

/// The three gates, or None when the request may proceed. An `Origin` at
/// all means a browser sent this; it is allowed only from the house's own
/// addresses, and `X-Homeostat` is required either way.
fn gate(host: Option<&str>, origin: Option<&str>, write_header: bool) -> Option<String> {
    if !host.is_some_and(host_allowed) {
        return Some("Host not allowed\n".to_string());
    }
    if origin.is_some_and(|o| !host_allowed(o)) {
        return Some("Origin not allowed\n".to_string());
    }
    if !write_header {
        return Some("missing X-Homeostat header\n".to_string());
    }
    None
}

fn respond(
    stream: &mut TcpStream,
    status: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> std::io::Result<()> {
    let mut out = format!("HTTP/1.1 {status}\r\n");
    for (name, value) in headers {
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    out.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    stream.write_all(out.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}
