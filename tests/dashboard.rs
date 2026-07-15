//! Dashboard service integration: the family's web surface against a live
//! supervised house (tests/fixture_house_dashboard).
//!
//! Success criteria (docs/design.md, Dashboard):
//! 1. `/api/model` renders the manifests: entities with capability,
//!    features and naming; units with family-editable params only.
//! 2. The WebSocket snapshot carries current bus state, and a command
//!    POSTed with the write header is published at the concrete cmd key —
//!    observed via the reflector echoing it back as state.
//! 3. A parameter write within constraints persists through the core's
//!    validating config queryable; an out-of-constraint write is refused
//!    and changes nothing.
//! 4. The local-only gate holds: a write without the X-Homeostat header
//!    and any request with a foreign Host are both refused.

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use homeostat::bus::HealthStatus;
use serde_json::{json, Value};

use common::{await_health, health_watch, Supervisor};

const FIXTURE: &str = "tests/fixture_house_dashboard";
const LAMP_STATE: &str = "home/state/livingroom/lamp/on";
const OFF_TIME_KEY: &str = "home/config/evening_lights/off_time";

fn http_request(
    addr: &str,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&Value>,
) -> (u16, Value) {
    let payload = body.map(|b| b.to_string()).unwrap_or_default();
    let mut request = format!("{method} {path} HTTP/1.1\r\nConnection: close\r\n");
    let mut has_host = false;
    for (name, value) in headers {
        has_host |= name.eq_ignore_ascii_case("host");
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    if !has_host {
        request.push_str(&format!("Host: {addr}\r\n"));
    }
    if body.is_some() {
        request.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            payload.len()
        ));
    }
    request.push_str("\r\n");
    request.push_str(&payload);

    let mut stream = TcpStream::connect(addr).expect("connect dashboard");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .expect("read timeout");
    stream.write_all(request.as_bytes()).expect("send request");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("read response");
    let response = String::from_utf8_lossy(&response).to_string();
    let status: u16 = response
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no status line in {response:?}"));
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, b)| b)
        .unwrap_or("");
    let value = if body.trim().is_empty() {
        Value::Null
    } else {
        serde_json::from_str(body.trim()).unwrap_or(Value::Null)
    };
    (status, value)
}

/// Minimal WebSocket client: handshake, then text frames on demand.
/// Server frames are unmasked.
fn ws_connect(addr: &str, path: &str) -> TcpStream {
    let mut stream = TcpStream::connect(addr).expect("connect ws");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .expect("read timeout");
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nUpgrade: websocket\r\n\
         Connection: Upgrade\r\nSec-WebSocket-Key: AAAAAAAAAAAAAAAAAAAAAA==\r\n\
         Sec-WebSocket-Version: 13\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).expect("ws handshake");

    // Read headers byte-wise up to CRLFCRLF, then frame bytes follow.
    let mut headers = Vec::new();
    let mut byte = [0u8; 1];
    while !headers.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).expect("handshake byte");
        headers.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&headers);
    assert!(
        head.starts_with("HTTP/1.1 101"),
        "expected 101 upgrade, got {head:?}"
    );
    stream
}

fn ws_read_message(stream: &mut TcpStream) -> Value {
    let mut prefix = [0u8; 2];
    stream.read_exact(&mut prefix).expect("frame header");
    assert_eq!(prefix[0] & 0x0f, 0x1, "expected a text frame");
    let mut len = (prefix[1] & 0x7f) as u64;
    if len == 126 {
        let mut ext = [0u8; 2];
        stream.read_exact(&mut ext).expect("extended len");
        len = u16::from_be_bytes(ext) as u64;
    } else if len == 127 {
        let mut ext = [0u8; 8];
        stream.read_exact(&mut ext).expect("extended len");
        len = u64::from_be_bytes(ext);
    }
    let mut payload = vec![0u8; len as usize];
    stream.read_exact(&mut payload).expect("frame payload");
    serde_json::from_slice(&payload).expect("frame is JSON")
}

/// Reads a concrete key from a core queryable (last-value cache).
async fn cache_read(session: &zenoh::Session, key: &str) -> Option<Value> {
    let replies = session.get(key).await.expect("cache read query");
    let mut value = None;
    while let Ok(reply) = replies.recv_async().await {
        if let Ok(sample) = reply.result() {
            value = serde_json::from_slice(&sample.payload().to_bytes()).ok();
        }
    }
    value
}

#[tokio::test(flavor = "multi_thread")]
async fn dashboard_serves_the_family_surface() {
    let port = common::free_port();
    let addr = format!("127.0.0.1:{port}");
    let mut sup =
        Supervisor::spawn_with_env(FIXTURE, &[("HOMEOSTAT_DASHBOARD_PORT", &port.to_string())]);
    let observer = sup.observer().await;

    // First run resolves the dashboard's uv environment (aiohttp): generous.
    let mut dash = health_watch(&observer, "dashboard").await;
    await_health(&mut dash, Duration::from_secs(180), |h| {
        h.status == HealthStatus::Running
    })
    .await;
    let mut reflector = health_watch(&observer, "reflector").await;
    await_health(&mut reflector, Duration::from_secs(10), |h| {
        h.status == HealthStatus::Running
    })
    .await;

    // 1. The model is the manifests, rendered.
    let (status, model) = http_request(&addr, "GET", "/api/model", &[], None);
    assert_eq!(status, 200, "{model}");
    let lamp = model["entities"]
        .as_array()
        .expect("entities")
        .iter()
        .find(|e| e["name"] == "lamp")
        .expect("lamp in model");
    assert_eq!(lamp["capability"], "light");
    assert_eq!(lamp["features"], json!(["brightness"]));
    assert_eq!(lamp["label"], "living room lamp");
    assert_eq!(lamp["room"], "livingroom");
    let evening = model["units"]
        .as_array()
        .expect("units")
        .iter()
        .find(|u| u["name"] == "evening_lights")
        .expect("evening_lights in model");
    assert_eq!(evening["params"]["off_time"]["type"], "time");
    assert_eq!(
        evening["params"]["off_time"]["constraint"]["after"],
        "20:00"
    );

    // The page itself is served.
    let (status, _) = http_request(&addr, "GET", "/", &[], None);
    assert_eq!(status, 200);

    // 2. Command path: POST -> manual-band publish -> reflector echoes state.
    let echo = observer
        .declare_subscriber(LAMP_STATE)
        .await
        .expect("state subscriber");
    let (status, reply) = http_request(
        &addr,
        "POST",
        "/api/cmd",
        &[("X-Homeostat", "family")],
        Some(&json!({"room": "livingroom", "entity": "lamp", "aspect": "on", "value": true})),
    );
    assert_eq!(status, 200, "{reply}");
    let sample = tokio::time::timeout(Duration::from_secs(10), echo.recv_async())
        .await
        .expect("reflector echoed the command")
        .expect("sample");
    let value: Value = serde_json::from_slice(&sample.payload().to_bytes()).expect("json");
    assert_eq!(value, json!(true));

    // A fresh WebSocket snapshot now carries the lamp state...
    let mut ws = ws_connect(&addr, "/ws");
    let snapshot = ws_read_message(&mut ws);
    assert_eq!(snapshot["type"], "snapshot");
    assert_eq!(snapshot["state"][LAMP_STATE], json!(true), "{snapshot}");
    assert!(
        snapshot["health"]["home/health/dashboard"]["status"].is_string(),
        "{snapshot}"
    );

    // ...and live deltas follow: another command's state echo reaches the
    // open socket (health deltas may interleave; scan a few frames).
    let (status, reply) = http_request(
        &addr,
        "POST",
        "/api/cmd",
        &[("X-Homeostat", "family")],
        Some(&json!({"room": "livingroom", "entity": "lamp", "aspect": "brightness", "value": 99})),
    );
    assert_eq!(status, 200, "{reply}");
    let delta = (0..10)
        .map(|_| ws_read_message(&mut ws))
        .find(|m| m["type"] == "state" && m["key"] == "home/state/livingroom/lamp/brightness")
        .expect("brightness delta on the open socket");
    assert_eq!(delta["value"], json!(99));

    // A command outside the vocabulary is refused before the bus.
    let (status, reply) = http_request(
        &addr,
        "POST",
        "/api/cmd",
        &[("X-Homeostat", "family")],
        Some(&json!({"room": "livingroom", "entity": "lamp", "aspect": "locked", "value": true})),
    );
    assert_eq!(status, 400, "{reply}");

    // 3. Parameter path: in-constraint persists, out-of-constraint refused.
    let (status, reply) = http_request(
        &addr,
        "POST",
        "/api/param",
        &[("X-Homeostat", "family")],
        Some(&json!({"unit": "evening_lights", "param": "off_time", "value": "21:30"})),
    );
    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply["value"], json!("21:30"));
    assert_eq!(
        cache_read(&observer, OFF_TIME_KEY).await,
        Some(json!("21:30"))
    );

    let (status, reply) = http_request(
        &addr,
        "POST",
        "/api/param",
        &[("X-Homeostat", "family")],
        Some(&json!({"unit": "evening_lights", "param": "off_time", "value": "03:00"})),
    );
    assert_eq!(status, 400, "out-of-constraint accepted: {reply}");
    assert!(reply["error"].is_string(), "{reply}");
    assert_eq!(
        cache_read(&observer, OFF_TIME_KEY).await,
        Some(json!("21:30")),
        "refused write must change nothing"
    );

    // 4. The local-only gate.
    let (status, _) = http_request(
        &addr,
        "POST",
        "/api/param",
        &[],
        Some(&json!({"unit": "evening_lights", "param": "off_time", "value": "21:00"})),
    );
    assert_eq!(status, 403, "write without X-Homeostat must be refused");
    let (status, _) = http_request(
        &addr,
        "GET",
        "/api/model",
        &[("Host", "dashboard.example.com")],
        None,
    );
    assert_eq!(status, 403, "foreign Host must be refused");

    sup.shutdown();
}
