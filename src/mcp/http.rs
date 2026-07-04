//! The HTTP transport: MCP streamable-HTTP in its stateless shape. A POST
//! carries one JSON-RPC message and gets the JSON response in the body
//! (the spec allows `application/json` in place of an SSE stream); a
//! notification gets 202 with no body; GET is 405 — this server never
//! initiates messages, so there is no stream to open and no session to
//! manage. Thread per connection: agent traffic is a conversation, not a
//! load profile.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

use serde_json::{json, Value};

use super::{protocol, Server};

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
            }
        }
        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body)?;

        if method != "POST" {
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
