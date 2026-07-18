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
//! 5. The map surface: vendored assets are served allowlisted, person
//!    entities render in `/api/model` but are never commandable, and
//!    `/tiles.pmtiles` 404s without HOMEOSTAT_DASHBOARD_TILES and serves
//!    Range requests when it is set (docs/design.md, "Map and person
//!    entities").

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use homeostat::bus::HealthStatus;
use serde_json::{json, Value};

use common::{await_health, health_watch, Supervisor};

const FIXTURE: &str = "tests/fixture_house_dashboard";
const LAMP_CMD: &str = "home/cmd/livingroom/lamp/on";
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

/// Like `http_request`, but keeps raw headers and body bytes instead of
/// parsing the body as JSON — needed to assert on Range/Content-Range for
/// the binary /tiles.pmtiles response.
fn http_request_bytes(
    addr: &str,
    path: &str,
    headers: &[(&str, &str)],
) -> (u16, Vec<(String, String)>, Vec<u8>) {
    let mut request = format!("GET {path} HTTP/1.1\r\nConnection: close\r\n");
    let mut has_host = false;
    for (name, value) in headers {
        has_host |= name.eq_ignore_ascii_case("host");
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    if !has_host {
        request.push_str(&format!("Host: {addr}\r\n"));
    }
    request.push_str("\r\n");

    let mut stream = TcpStream::connect(addr).expect("connect dashboard");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .expect("read timeout");
    stream.write_all(request.as_bytes()).expect("send request");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("read response");

    let split_at = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("header/body split");
    let header_block = String::from_utf8_lossy(&response[..split_at]).to_string();
    let body = response[split_at + 4..].to_vec();

    let mut lines = header_block.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no status line in {status_line:?}"));
    let headers = lines
        .filter_map(|l| l.split_once(": ").map(|(k, v)| (k.to_string(), v.to_string())))
        .collect();

    (status, headers, body)
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

    // A person entity is just another entity in the model, on the
    // reserved "person" pseudo-room — never commandable (checked below).
    let person = model["entities"]
        .as_array()
        .expect("entities")
        .iter()
        .find(|e| e["name"] == "family_member")
        .expect("family_member in model");
    assert_eq!(person["capability"], "person");
    assert_eq!(person["room"], "person");
    assert_eq!(person["label"], "family member");

    // No HOMEOSTAT_DASHBOARD_TILES in this fixture: tiles are unavailable.
    assert_eq!(model["tiles"], json!(false), "{model}");

    // The page itself is served.
    let (status, _) = http_request(&addr, "GET", "/", &[], None);
    assert_eq!(status, 200);

    // Vendored map assets are served, allowlisted by filename.
    for name in ["leaflet.js", "leaflet.css", "protomaps-leaflet.js"] {
        let (status, _) = http_request(&addr, "GET", &format!("/assets/{name}"), &[], None);
        assert_eq!(status, 200, "asset {name}");
    }
    let (status, _) = http_request(&addr, "GET", "/assets/dashboard.py", &[], None);
    assert_eq!(status, 404, "unlisted asset must 404");

    // No tiles configured: the endpoint 404s rather than pretending.
    let (status, _) = http_request(&addr, "GET", "/tiles.pmtiles", &[], None);
    assert_eq!(status, 404, "tiles.pmtiles without HOMEOSTAT_DASHBOARD_TILES");

    // 2. Command path: POST -> a manual-band envelope on home/cmd (priority
    // and actor stamped server-side) -> reflector echoes the value as state.
    let cmd_sub = observer
        .declare_subscriber(LAMP_CMD)
        .await
        .expect("cmd subscriber");
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
    let cmd_sample = tokio::time::timeout(Duration::from_secs(10), cmd_sub.recv_async())
        .await
        .expect("cmd envelope observed within 10s")
        .expect("sample");
    let envelope: Value = serde_json::from_slice(&cmd_sample.payload().to_bytes()).expect("json");
    assert_eq!(
        envelope,
        json!({"value": true, "priority": "manual", "actor": "dashboard"}),
        "the dashboard stamps its own manifest priority and unit name"
    );
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

    // A person entity is never commandable: not in COMMANDABLE at all.
    let (status, reply) = http_request(
        &addr,
        "POST",
        "/api/cmd",
        &[("X-Homeostat", "family")],
        Some(&json!({"room": "person", "entity": "family_member", "aspect": "lat", "value": 1.0})),
    );
    assert_eq!(status, 400, "person entity must refuse commands: {reply}");

    // A lock command is accepted: COMMANDABLE now maps lock -> {"locked"}.
    // The wish still just goes to home/cmd at manual band, stamped the same
    // way as any other command — for a real arbitrated entity, the arbiter
    // (not exercised by this fixture) is what enforces the family always
    // winning over automations (docs/design.md, Arbitrated mode).
    let lock_cmd_sub = observer
        .declare_subscriber("home/cmd/livingroom/front_door/locked")
        .await
        .expect("lock cmd subscriber");
    let (status, reply) = http_request(
        &addr,
        "POST",
        "/api/cmd",
        &[("X-Homeostat", "family")],
        Some(&json!({"room": "livingroom", "entity": "front_door", "aspect": "locked", "value": true})),
    );
    assert_eq!(status, 200, "{reply}");
    let cmd_sample = tokio::time::timeout(Duration::from_secs(10), lock_cmd_sub.recv_async())
        .await
        .expect("lock cmd envelope observed within 10s")
        .expect("sample");
    let envelope: Value = serde_json::from_slice(&cmd_sample.payload().to_bytes()).expect("json");
    assert_eq!(
        envelope,
        json!({"value": true, "priority": "manual", "actor": "dashboard"}),
        "lock command stamps the same manual-band envelope as any other command"
    );

    // A switch command is accepted too: COMMANDABLE now maps switch -> {"on"}
    // (a reflashed Sonoff relay is toggleable from the dashboard).
    let switch_cmd_sub = observer
        .declare_subscriber("home/cmd/livingroom/relay/on")
        .await
        .expect("switch cmd subscriber");
    let (status, reply) = http_request(
        &addr,
        "POST",
        "/api/cmd",
        &[("X-Homeostat", "family")],
        Some(&json!({"room": "livingroom", "entity": "relay", "aspect": "on", "value": true})),
    );
    assert_eq!(status, 200, "{reply}");
    let cmd_sample = tokio::time::timeout(Duration::from_secs(10), switch_cmd_sub.recv_async())
        .await
        .expect("switch cmd envelope observed within 10s")
        .expect("sample");
    let envelope: Value = serde_json::from_slice(&cmd_sample.payload().to_bytes()).expect("json");
    assert_eq!(
        envelope,
        json!({"value": true, "priority": "manual", "actor": "dashboard"}),
        "switch command stamps the same manual-band envelope as any other command"
    );

    // A climate command is accepted: COMMANDABLE now maps climate ->
    // {"setpoint"}, the family-facing base aspect (docs/design.md, IVT490
    // heat-pump adapter, "Climate vocabulary"). Setpoint is a float — the
    // envelope must carry it through unmodified, same as any other value.
    let climate_cmd_sub = observer
        .declare_subscriber("home/cmd/livingroom/heat_pump/setpoint")
        .await
        .expect("climate cmd subscriber");
    let (status, reply) = http_request(
        &addr,
        "POST",
        "/api/cmd",
        &[("X-Homeostat", "family")],
        Some(&json!({"room": "livingroom", "entity": "heat_pump", "aspect": "setpoint", "value": 21.5})),
    );
    assert_eq!(status, 200, "{reply}");
    let cmd_sample = tokio::time::timeout(Duration::from_secs(10), climate_cmd_sub.recv_async())
        .await
        .expect("climate cmd envelope observed within 10s")
        .expect("sample");
    let envelope: Value = serde_json::from_slice(&cmd_sample.payload().to_bytes()).expect("json");
    assert_eq!(
        envelope,
        json!({"value": 21.5, "priority": "manual", "actor": "dashboard"}),
        "climate setpoint command stamps the same manual-band envelope, float value intact"
    );

    // Every other climate aspect is read-only passthrough: not commandable.
    let (status, reply) = http_request(
        &addr,
        "POST",
        "/api/cmd",
        &[("X-Homeostat", "family")],
        Some(&json!({"room": "livingroom", "entity": "heat_pump", "aspect": "feed_temperature_target", "value": 45.0})),
    );
    assert_eq!(status, 400, "non-commandable climate aspect must be refused: {reply}");

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

/// 5. With HOMEOSTAT_DASHBOARD_TILES set, /tiles.pmtiles serves the file —
/// in full, and as a Range-respecting partial response — and /api/model
/// says so.
#[tokio::test(flavor = "multi_thread")]
async fn dashboard_serves_configured_tiles() {
    let port = common::free_port();
    let addr = format!("127.0.0.1:{port}");
    let tiles_path =
        std::env::temp_dir().join(format!("homeostat-dashboard-tiles-{port}.pmtiles"));
    let contents = b"fake-pmtiles-bytes-0123456789";
    std::fs::write(&tiles_path, contents).expect("write fixture tiles file");

    let mut sup = Supervisor::spawn_with_env(
        FIXTURE,
        &[
            ("HOMEOSTAT_DASHBOARD_PORT", &port.to_string()),
            (
                "HOMEOSTAT_DASHBOARD_TILES",
                tiles_path.to_str().expect("utf-8 path"),
            ),
        ],
    );
    let observer = sup.observer().await;
    let mut dash = health_watch(&observer, "dashboard").await;
    await_health(&mut dash, Duration::from_secs(180), |h| {
        h.status == HealthStatus::Running
    })
    .await;

    let (status, model) = http_request(&addr, "GET", "/api/model", &[], None);
    assert_eq!(status, 200, "{model}");
    assert_eq!(model["tiles"], json!(true), "{model}");

    let (status, headers, body) = http_request_bytes(&addr, "/tiles.pmtiles", &[]);
    assert_eq!(status, 200);
    assert_eq!(body, contents);
    assert!(
        headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("accept-ranges") && v == "bytes"),
        "{headers:?}"
    );

    let (status, headers, body) = http_request_bytes(&addr, "/tiles.pmtiles", &[("Range", "bytes=0-4")]);
    assert_eq!(status, 206, "{headers:?}");
    assert_eq!(body, &contents[..5]);
    assert!(
        headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-range")),
        "{headers:?}"
    );

    sup.shutdown();
    let _ = std::fs::remove_file(&tiles_path);
}
