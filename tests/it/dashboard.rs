//! Dashboard service integration: the family's web surface against a live
//! supervised house (`tests/fixtures/house_dashboard`).
//!
//! Success criteria (docs/design.md#dashboard):
//! 1. `/api/model` renders the manifests: entities with capability,
//!    features and naming; units with every param and its `editable_by`,
//!    of which only family ones are writable.
//! 2. The WebSocket snapshot carries current bus state, and a command
//!    posted with the write header is published at the concrete cmd key.
//!    The test sees it through the reflector echoing it back as state. A
//!    command for a capability the dashboard's own manifest does not grant
//!    is refused before the bus, and the model marks the entity not
//!    commandable. An aspect descriptor in the owning adapter's discovery
//!    record admits the family-editable commands it declares, within their
//!    constraints, and rides the snapshot
//!    (docs/design.md#aspect-descriptors).
//! 3. A parameter write within constraints persists through the core's
//!    validating config queryable; an out-of-constraint write is refused
//!    and changes nothing.
//! 4. The local-only gate holds: a write without the X-Homeostat header
//!    and any request with a foreign Host are both refused.
//! 5. The map surface: vendored assets are served allowlisted (the page's
//!    modules by a path inside `assets/dashboard/`, nothing outside), person
//!    entities render in `/api/model` but are not commandable, and
//!    `/tiles.pmtiles` 404s without `HOMEOSTAT_DASHBOARD_TILES` and serves
//!    Range requests when it is set (docs/design.md#map-and-people).
//! 6. `/api/logs` proxies a unit's captured stdout/stderr tail
//!    (docs/design.md#logs-and-the-audit-trail): known lines come back
//!    stream-tagged, `lines=N` truncates, and an unknown unit 404s.
//! 7. The camera media plane (docs/design.md#cameras). Browsers do not
//!    speak go2rtc. `/api/camera/{entity}/live` relays the MSE WebSocket
//!    byte for byte and addresses the stream by the camera's entity id
//!    (how the shim names it) rather than its entity name. An unknown or
//!    non-camera entity 404s. There is no snapshot route: a still frame
//!    needs a transcode the media plane does not carry.

use crate::common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use homeostat::bus;
use serde_json::{json, Value};

use crate::common::{
    await_running, await_sample, cache_read, eventually, expect_silence, http, next_sample, up,
    FakeServer, HttpResponse, StateSub, Supervisor, FIRST_RUN, SILENCE,
};

const FIXTURE: &str = "tests/fixtures/house_dashboard";
const LAMP_CMD: &str = "home/cmd/livingroom/lamp/on";
const LAMP_STATE: &str = "home/state/livingroom/lamp/on";
const OFF_TIME_KEY: &str = "home/config/evening_lights/off_time";

/// One request with an optional JSON body. The reply body is parsed as
/// JSON, or Null when it is empty or not JSON.
fn http_request(
    addr: &str,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&Value>,
) -> (u16, Value) {
    let payload = body.map(Value::to_string);
    let mut headers = headers.to_vec();
    if payload.is_some() {
        headers.push(("Content-Type", "application/json"));
    }
    let response = http(
        addr,
        method,
        path,
        &headers,
        payload.as_deref().map(str::as_bytes),
    );
    let value = serde_json::from_slice(&response.body).unwrap_or(Value::Null);
    (response.status, value)
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
    let mut len = u64::from(prefix[1] & 0x7f);
    if len == 126 {
        let mut ext = [0u8; 2];
        stream.read_exact(&mut ext).expect("extended len");
        len = u64::from(u16::from_be_bytes(ext));
    } else if len == 127 {
        let mut ext = [0u8; 8];
        stream.read_exact(&mut ext).expect("extended len");
        len = u64::from_be_bytes(ext);
    }
    let mut payload = vec![0u8; len as usize];
    stream.read_exact(&mut payload).expect("frame payload");
    serde_json::from_slice(&payload).expect("frame is JSON")
}

/// The browser suite (tests/browser) runs the real page against canned
/// fixtures. That makes it fast and deterministic, and also lets it drift:
/// hand-maintained JSON can stop looking like what this unit emits while
/// every test above keeps passing. This test compares the field names of a
/// real `/api/model` against the fixture's, and fails when the real one
/// has a field the fixture lacks.
///
/// It compares names and not values, because the fixture house is a
/// different house and only the shape has to match. `vocabulary` and
/// `limits` are the same in every house, so their values must match too.
#[tokio::test(flavor = "multi_thread")]
async fn browser_fixtures_still_look_like_the_real_model() {
    let port = common::free_port();
    let addr = format!("127.0.0.1:{port}");
    let (mut sup, _observer) = up(
        || Supervisor::spawn_with_env(FIXTURE, &[("HOMEOSTAT_DASHBOARD_PORT", &port.to_string())]),
        &["dashboard"],
    )
    .await;

    let (status, model) = http_request(&addr, "GET", "/api/model", &[], None);
    assert_eq!(status, 200);

    let fixture: Value = serde_json::from_str(include_str!("../browser/fixtures/model.json"))
        .expect("browser fixture model is JSON");

    let names =
        |v: &Value| -> Vec<String> { v.as_object().expect("an object").keys().cloned().collect() };
    let missing = |real: &Value, canned: &Value, what: &str| {
        let canned_names = names(canned);
        for field in names(real) {
            assert!(
                canned_names.contains(&field),
                "{what}: /api/model carries \"{field}\", the browser fixture does not — \
                 tests/browser/fixtures/model.json has drifted from what the unit emits"
            );
        }
    };

    missing(&model, &fixture, "model");
    missing(&model["entities"][0], &fixture["entities"][0], "an entity");
    missing(&model["units"][0], &fixture["units"][0], "a unit");
    for block in ["vocabulary", "limits"] {
        assert_eq!(
            model[block], fixture[block],
            "/api/model's \"{block}\" differs from tests/browser/fixtures/model.json's"
        );
    }

    sup.shutdown();
}

/// What the sections of `dashboard_serves_the_family_surface` share: the
/// dashboard's address, the observer session and the bus subscribers
/// declared before the units start.
struct Ctx {
    addr: String,
    observer: zenoh::Session,
    cmd_sub: StateSub,
    echo: StateSub,
    lock_cmd_sub: StateSub,
    garage_sub: StateSub,
    switch_cmd_sub: StateSub,
    climate_cmd_sub: StateSub,
    mode_cmd_sub: StateSub,
    notify_cmd_sub: StateSub,
}

#[tokio::test(flavor = "multi_thread")]
async fn dashboard_serves_the_family_surface() {
    let port = common::free_port();
    let addr = format!("127.0.0.1:{port}");
    let permit = common::startup_permit().await;
    let mut sup =
        Supervisor::spawn_with_env(FIXTURE, &[("HOMEOSTAT_DASHBOARD_PORT", &port.to_string())]);
    let observer = sup.observer().await;

    // All bus subscribers are declared up front, before the health waits,
    // rather than right before the POST that exercises them. A subscriber's
    // interest needs to propagate through the router to the publishing
    // session, and a put racing that propagation is dropped without notice
    // (often, under CI load). The multi-second startup below gives it time
    // to settle.
    let cmd_sub = observer
        .declare_subscriber(LAMP_CMD)
        .await
        .expect("cmd subscriber");
    let echo = observer
        .declare_subscriber(LAMP_STATE)
        .await
        .expect("state subscriber");
    let lock_cmd_sub = observer
        .declare_subscriber("home/cmd/livingroom/front_door/locked")
        .await
        .expect("lock cmd subscriber");
    let garage_sub = observer
        .declare_subscriber("home/cmd/livingroom/garage_door/locked")
        .await
        .expect("garage cmd subscriber");
    let switch_cmd_sub = observer
        .declare_subscriber("home/cmd/livingroom/relay/on")
        .await
        .expect("switch cmd subscriber");
    let climate_cmd_sub = observer
        .declare_subscriber("home/cmd/livingroom/heat_pump/setpoint")
        .await
        .expect("climate cmd subscriber");
    let mode_cmd_sub = observer
        .declare_subscriber("home/cmd/livingroom/heat_pump/*")
        .await
        .expect("mode cmd subscriber");
    let notify_cmd_sub = observer
        .declare_subscriber("home/cmd/person/phone/*")
        .await
        .expect("notifier cmd subscriber");

    // First run resolves the dashboard's uv environment (aiohttp): generous.
    await_running(&observer, "dashboard", FIRST_RUN).await;
    await_running(&observer, "reflector", Duration::from_secs(10)).await;
    drop(permit);

    let ctx = Ctx {
        addr,
        observer,
        cmd_sub,
        echo,
        lock_cmd_sub,
        garage_sub,
        switch_cmd_sub,
        climate_cmd_sub,
        mode_cmd_sub,
        notify_cmd_sub,
    };
    let model = model_renders_the_manifests(&ctx);
    model_carries_views_and_relations(&ctx, &model).await;
    page_and_assets_are_served(&ctx);
    let mut ws = command_reaches_the_bus_and_the_socket(&ctx).await;
    malformed_commands_stay_off_the_bus(&ctx).await;
    lock_command_is_stamped_at_manual_band(&ctx).await;
    holds_reach_the_bus_and_the_page(&ctx, &mut ws).await;
    let model = grants_decide_what_is_commandable(&ctx).await;
    climate_takes_only_its_setpoint(&ctx).await;
    notifier_takes_its_text(&ctx, &model).await;
    aspect_descriptors_admit_family_commands(&ctx).await;
    params_persist_within_constraints(&ctx).await;
    local_only_gate_holds(&ctx);
    history_checks_its_selector(&ctx);
    windows_refuse_bad_hours(&ctx);

    sup.shutdown();
}

/// 1. The model is the manifests, rendered.
fn model_renders_the_manifests(ctx: &Ctx) -> Value {
    let (status, model) = http_request(&ctx.addr, "GET", "/api/model", &[], None);
    assert_eq!(status, 200, "{model}");
    // Where the page is served from: the core's version off the bus, and
    // the SDK this unit runs against (the release its page came from).
    assert_eq!(
        model["about"]["homeostat"]["version"],
        json!(env!("CARGO_PKG_VERSION")),
        "{model}"
    );
    assert!(
        model["about"]["dashboard"]["version"].is_string(),
        "{model}"
    );
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
    assert_eq!(evening["params"]["off_time"]["editable_by"], "family");
    // Owner params are in the model too, carrying their tier, so the page
    // can show them read-only and count them as deviations when off their
    // default. Visibility is house-wide; only the write is gated.
    assert_eq!(evening["params"]["grace_minutes"]["default"], json!(5));
    assert_eq!(evening["params"]["grace_minutes"]["editable_by"], "owner");

    // A tracker is an ordinary entity in the model, on the reserved
    // "person" pseudo-room. It is not commandable (checked below).
    let person = model["entities"]
        .as_array()
        .expect("entities")
        .iter()
        .find(|e| e["name"] == "family_member")
        .expect("family_member in model");
    assert_eq!(person["capability"], "tracker");
    assert_eq!(person["room"], "person");
    assert_eq!(person["label"], "family member");

    // No HOMEOSTAT_DASHBOARD_TILES in this fixture: tiles are unavailable.
    assert_eq!(model["tiles"], json!(false), "{model}");
    model
}

/// 1b. dashboard.toml rides the model as written (the core validated
/// it), and each unit carries the relations its card draws: what it
/// drives, from the grant table, and what it reads, from its
/// subscriptions resolved against live state keys
/// (docs/design.md#views-are-text).
async fn model_carries_views_and_relations(ctx: &Ctx, model: &Value) {
    let evening = model["units"]
        .as_array()
        .expect("units")
        .iter()
        .find(|u| u["name"] == "evening_lights")
        .expect("evening_lights in model");
    assert_eq!(model["views"][0]["name"], "downstairs", "{model}");
    assert_eq!(model["views"][0]["widgets"][0]["kind"], "unit");
    assert_eq!(model["views"][1]["kind"], "rooms");
    assert_eq!(
        evening["drives"],
        json!([{"entity": "lamp", "aspect": "on"}]),
        "the light grant over home/cmd/downstairs/*/on resolves onto the lamp's `on`"
    );
    assert_eq!(
        evening["subscribes"]["presence"],
        "home/state/downstairs/*/occupied"
    );
    ctx.observer
        .put(
            "home/state/livingroom/presence_sensor/occupied",
            json!(true).to_string(),
        )
        .await
        .expect("presence publish");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (_, model) = http_request(&ctx.addr, "GET", "/api/model", &[], None);
        let evening = model["units"]
            .as_array()
            .expect("units")
            .iter()
            .find(|u| u["name"] == "evening_lights")
            .expect("evening_lights in model")
            .clone();
        // The zone-keyed subscription reaches the livingroom sensor once
        // its key is on the bus. The lamp's `on` is subscribed too but not
        // published yet at this point, and a source is a live key rather than
        // a subscription.
        if evening["sources"] == json!([{"entity": "presence_sensor", "aspect": "occupied"}]) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "sources never resolved: {}",
            evening["sources"]
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// The page and the assets it loads are served. Paths outside the
/// asset allowlist 404, and so do tiles this fixture does not configure.
fn page_and_assets_are_served(ctx: &Ctx) {
    // The page itself is served.
    let (status, _) = http_request(&ctx.addr, "GET", "/", &[], None);
    assert_eq!(status, 200);

    // ...and tells the browser to revalidate it. The page and its logic
    // asset are written against each other. Without revalidation the
    // browser uses heuristic freshness, and an upgrade can pair the new page
    // with the cached old logic. The result is an empty page that takes no
    // taps, with a healthy backend behind it (docs/design.md#the-page).
    let revalidates = |path: &str| {
        let HttpResponse {
            status, headers, ..
        } = http(&ctx.addr, "GET", path, &[], None);
        assert_eq!(status, 200, "{path}");
        let cache_control = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("cache-control"))
            .map_or_else(
                || panic!("{path} served without Cache-Control"),
                |(_, value)| value.to_ascii_lowercase(),
            );
        assert!(
            cache_control.contains("no-cache"),
            "{path}: Cache-Control {cache_control:?} lets a browser skip revalidation"
        );
    };
    revalidates("/");
    revalidates("/assets/dashboard-logic.js");

    // The page's own logic and stylesheet are served, allowlisted by
    // filename.
    for name in ["dashboard-logic.js", "dashboard.css", "homeostat-mark.svg"] {
        let (status, _) = http_request(&ctx.addr, "GET", &format!("/assets/{name}"), &[], None);
        assert_eq!(status, 200, "asset {name}");
    }
    let (status, _) = http_request(&ctx.addr, "GET", "/assets/dashboard.py", &[], None);
    assert_eq!(status, 404, "unlisted asset must 404");

    // The page's modules are served from assets/dashboard/, nested too, as
    // JavaScript (a browser refuses to run a module served as anything
    // else) and revalidated like the rest of the page.
    for path in [
        "/assets/dashboard/main.js",
        "/assets/dashboard/widgets/entity.js",
    ] {
        revalidates(path);
        let HttpResponse { headers, .. } = http(&ctx.addr, "GET", path, &[], None);
        let content_type = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            .map(|(_, value)| value.as_str());
        assert_eq!(content_type, Some("text/javascript"), "{path}");
    }
    // ...and nothing outside it: demo-site/shim.js is a real script in the
    // repo, three directories up from the modules.
    for path in [
        "/assets/dashboard/../../../demo-site/shim.js",
        "/assets/dashboard/%2e%2e/%2e%2e/%2e%2e/demo-site/shim.js",
        "/assets/dashboard/nope.js",
        "/assets/dashboard/%00.js",
    ] {
        let HttpResponse { status, .. } = http(&ctx.addr, "GET", path, &[], None);
        assert_eq!(status, 404, "{path} must 404");
    }

    // The vendored map files are served from assets/map/ by suffix, each
    // as the type the browser needs: a module as JavaScript, a font range
    // as protobuf, an icon sheet as an image. A font directory has spaces
    // in its name, which the URL escapes.
    for (path, expected) in [
        ("/assets/map/maplibre-gl.mjs", "text/javascript"),
        ("/assets/map/maplibre-gl-worker.mjs", "text/javascript"),
        ("/assets/map/maplibre-gl.css", "text/css"),
        ("/assets/map/pmtiles.js", "text/javascript"),
        ("/assets/map/basemaps.mjs", "text/javascript"),
        ("/assets/map/sprites/light.json", "application/json"),
        ("/assets/map/sprites/dark@2x.png", "image/png"),
        (
            "/assets/map/fonts/Noto%20Sans%20Regular/0-255.pbf",
            "application/x-protobuf",
        ),
    ] {
        let HttpResponse {
            status, headers, ..
        } = http(&ctx.addr, "GET", path, &[], None);
        assert_eq!(status, 200, "{path}");
        let content_type = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            .map(|(_, value)| value.as_str());
        assert_eq!(content_type, Some(expected), "{path}");
    }
    // ...and nothing outside it: adapters/dashboard.py and
    // adapters/assets/README.md are real files beside it.
    for path in [
        "/assets/map/../../dashboard.py",
        "/assets/map/%2e%2e/%2e%2e/dashboard.py",
        "/assets/map/../README.md",
        "/assets/map/nope.png",
    ] {
        let HttpResponse { status, .. } = http(&ctx.addr, "GET", path, &[], None);
        assert_eq!(status, 404, "{path} must 404");
    }

    // No tiles configured: the endpoint 404s.
    let (status, _) = http_request(&ctx.addr, "GET", "/tiles.pmtiles", &[], None);
    assert_eq!(
        status, 404,
        "tiles.pmtiles without HOMEOSTAT_DASHBOARD_TILES"
    );
}

/// 2. Command path: POST -> a manual-band envelope on home/cmd (priority
///    and actor stamped server-side) -> reflector echoes the value as state.
///
/// Returns the open WebSocket, which later sections read deltas from.
async fn command_reaches_the_bus_and_the_socket(ctx: &Ctx) -> TcpStream {
    let (status, reply) = http_request(
        &ctx.addr,
        "POST",
        "/api/cmd",
        &[("X-Homeostat", "family")],
        Some(&json!({"room": "livingroom", "entity": "lamp", "aspect": "on", "value": true})),
    );
    assert_eq!(status, 200, "{reply}");
    let envelope = next_sample(&ctx.cmd_sub, Duration::from_secs(30))
        .await
        .expect("cmd envelope observed within 10s")
        .value()
        .clone();
    let cmd_id = envelope["id"]
        .as_str()
        .expect("envelope carries a correlation id");
    assert_eq!(
        envelope,
        json!({"value": true, "priority": "manual", "actor": "dashboard", "id": cmd_id}),
        "the dashboard stamps its own manifest priority and unit name"
    );
    // The browser gets that same id back, so the control can show the
    // command pending and then resolve it against the event that ends it.
    assert_eq!(
        reply,
        json!({"ok": true, "id": cmd_id, "heard": true}),
        "the id the caller is handed is the id that went on the bus"
    );
    let echoed = next_sample(&ctx.echo, Duration::from_secs(30))
        .await
        .expect("reflector echoed the command");
    assert_eq!(echoed.value(), &json!(true));

    // A fresh WebSocket snapshot now carries the lamp state. The echo we
    // observed above proves the reflector published it, but router fan-out
    // to the dashboard's own subscriber is unordered with respect to ours.
    // The test reconnects until the dashboard's cache has caught up.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let (mut ws, snapshot) = loop {
        let mut ws = ws_connect(&ctx.addr, "/ws");
        let snapshot = ws_read_message(&mut ws);
        assert_eq!(snapshot["type"], "snapshot");
        if snapshot["state"][LAMP_STATE] == json!(true) {
            break (ws, snapshot);
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "snapshot never carried the lamp state: {snapshot}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert!(
        snapshot["health"]["home/health/dashboard"]["status"].is_string(),
        "{snapshot}"
    );

    // ...and live deltas follow: another command's state echo reaches the
    // open socket (health deltas may interleave; scan a few frames).
    let (status, reply) = http_request(
        &ctx.addr,
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
    ws
}

/// Commands the vocabulary does not admit, a tracker's included, and values
/// of the wrong type are refused before the bus.
async fn malformed_commands_stay_off_the_bus(ctx: &Ctx) {
    // A command outside the vocabulary is refused before the bus.
    let (status, reply) = http_request(
        &ctx.addr,
        "POST",
        "/api/cmd",
        &[("X-Homeostat", "family")],
        Some(&json!({"room": "livingroom", "entity": "lamp", "aspect": "locked", "value": true})),
    );
    assert_eq!(status, 400, "{reply}");

    // A tracker is not commandable: it is not in COMMANDABLE.
    let (status, reply) = http_request(
        &ctx.addr,
        "POST",
        "/api/cmd",
        &[("X-Homeostat", "family")],
        Some(&json!({"room": "person", "entity": "family_member", "aspect": "lat", "value": 1.0})),
    );
    assert_eq!(status, 400, "a tracker must refuse commands: {reply}");

    // A vocabulary aspect is type-checked before the bus: a lock takes a
    // bool, and a string, an object or NaN is refused. A body past the
    // 64 KiB cap does not reach the handler (aiohttp answers 413).
    let (status, reply) = http_request(
        &ctx.addr,
        "POST",
        "/api/cmd",
        &[("X-Homeostat", "family")],
        Some(
            &json!({"room": "livingroom", "entity": "front_door", "aspect": "locked", "value": "yes"}),
        ),
    );
    assert_eq!(status, 400, "a string is not a lock value: {reply}");
    let (status, reply) = http_request(
        &ctx.addr,
        "POST",
        "/api/cmd",
        &[("X-Homeostat", "family")],
        Some(
            &json!({"room": "livingroom", "entity": "lamp", "aspect": "brightness", "value": true}),
        ),
    );
    assert_eq!(status, 400, "a bool is not a brightness: {reply}");
    let (status, _) = http_request(
        &ctx.addr,
        "POST",
        "/api/cmd",
        &[("X-Homeostat", "family")],
        Some(
            &json!({"room": "livingroom", "entity": "front_door", "aspect": "locked", "value": {"blob": "x".repeat(96 * 1024)}}),
        ),
    );
    assert_eq!(
        status, 413,
        "a body past the cap is refused before the handler"
    );
    // A body that is valid JSON but not an object is a 400, not a 500.
    let (status, reply) = http_request(
        &ctx.addr,
        "POST",
        "/api/cmd",
        &[("X-Homeostat", "family")],
        Some(&json!([1])),
    );
    assert_eq!(status, 400, "a JSON array body: {reply}");
    // A mistyped or oversized command does not reach the bus.
    expect_silence(&ctx.lock_cmd_sub, SILENCE, "lock command").await;
}

/// A lock command is accepted: COMMANDABLE maps lock -> {"locked"}.
/// The wish goes to home/cmd at manual band, stamped like any other
/// command. For a real arbitrated entity, the arbiter (not exercised by
/// this fixture) makes the family win over automations
/// (docs/design.md#commanding).
async fn lock_command_is_stamped_at_manual_band(ctx: &Ctx) {
    let (status, reply) = http_request(
        &ctx.addr,
        "POST",
        "/api/cmd",
        &[("X-Homeostat", "family")],
        Some(
            &json!({"room": "livingroom", "entity": "front_door", "aspect": "locked", "value": true}),
        ),
    );
    assert_eq!(status, 200, "{reply}");
    let envelope = next_sample(&ctx.lock_cmd_sub, Duration::from_secs(30))
        .await
        .expect("lock cmd envelope observed within 10s")
        .value()
        .clone();
    let cmd_id = envelope["id"]
        .as_str()
        .expect("envelope carries a correlation id");
    assert_eq!(
        envelope,
        json!({"value": true, "priority": "manual", "actor": "dashboard", "id": cmd_id}),
        "lock command stamps the same manual-band envelope as any other command"
    );
}

/// Hold intents are refused on a shared entity and sent on an arbitrated
/// one, and hold records reach the open socket.
async fn holds_reach_the_bus_and_the_page(ctx: &Ctx, ws: &mut TcpStream) {
    // A hold intent is for an arbitrated entity, and the page never says
    // what a device did (docs/design.md#hold-intents). Both are refused
    // before the bus; front_door is shared.
    for body in [
        json!({"room": "livingroom", "entity": "front_door", "aspect": "locked", "hold": "release"}),
        json!({"room": "livingroom", "entity": "front_door", "aspect": "locked", "value": true,
               "hold": "2099-01-01T06:00:00Z"}),
        json!({"room": "livingroom", "entity": "front_door", "aspect": "locked", "value": true,
               "hold": "report"}),
    ] {
        let (status, reply) = http_request(
            &ctx.addr,
            "POST",
            "/api/cmd",
            &[("X-Homeostat", "family")],
            Some(&body),
        );
        assert_eq!(status, 400, "{body}: {reply}");
    }
    // A refused hold does not reach the bus.
    expect_silence(&ctx.lock_cmd_sub, SILENCE, "lock command").await;

    // On the arbitrated garage door the dashboard sends a release with no
    // value, and a value held until an instant. It still never reports.
    let until = common::rfc3339_in(3600);
    for (body, expected) in [
        (
            json!({"room": "livingroom", "entity": "garage_door", "aspect": "locked", "hold": "release"}),
            json!({"priority": "manual", "actor": "dashboard", "hold": "release"}),
        ),
        (
            json!({"room": "livingroom", "entity": "garage_door", "aspect": "locked", "value": true,
                   "hold": until}),
            json!({"value": true, "priority": "manual", "actor": "dashboard", "hold": until}),
        ),
    ] {
        let (status, reply) = http_request(
            &ctx.addr,
            "POST",
            "/api/cmd",
            &[("X-Homeostat", "family")],
            Some(&body),
        );
        assert_eq!(status, 200, "{body}: {reply}");
        let mut envelope = next_sample(&ctx.garage_sub, Duration::from_secs(10))
            .await
            .expect("garage envelope observed")
            .value()
            .clone();
        assert_eq!(envelope["id"], reply["id"], "{reply}");
        envelope.as_object_mut().expect("an object").remove("id");
        assert_eq!(envelope, expected);
    }
    let (status, reply) = http_request(
        &ctx.addr,
        "POST",
        "/api/cmd",
        &[("X-Homeostat", "family")],
        Some(
            &json!({"room": "livingroom", "entity": "garage_door", "aspect": "locked",
                     "value": true, "hold": "report"}),
        ),
    );
    assert_eq!(status, 400, "the page never saw the device act: {reply}");

    // Holds reach the page one record per aspect, and a deleted key
    // reaches it as a null, so the page forgets a hold that ended.
    let hold_key = "home/hold/arbiter/livingroom/front_door/locked";
    let record = json!({"schema": 1, "room": "livingroom", "entity": "front_door",
        "aspect": "locked", "priority": "manual", "actor": "dashboard", "value": true,
        "since": "2026-11-16T10:00:00Z", "until": "2099-01-01T00:00:00Z", "refused": 0});
    ctx.observer
        .put(hold_key, record.to_string())
        .await
        .expect("hold put");
    let delta = (0..20)
        .map(|_| ws_read_message(ws))
        .find(|m| m["type"] == "hold" && m["key"] == hold_key)
        .expect("hold delta on the open socket");
    assert_eq!(delta["value"], record);
    ctx.observer.delete(hold_key).await.expect("hold delete");
    let delta = (0..20)
        .map(|_| ws_read_message(ws))
        .find(|m| m["type"] == "hold" && m["key"] == hold_key)
        .expect("hold delete on the open socket");
    assert_eq!(delta["value"], Value::Null);
}

/// A switch command is refused: the capability is in COMMANDABLE, but
/// this dashboard's manifest (units/dashboard.toml) grants light, lock
/// and climate and not switch. The unit honours the grant table `plan`
/// prints. The model says so too, so the page renders the relay
/// read-only rather than as a toggle that does nothing.
async fn grants_decide_what_is_commandable(ctx: &Ctx) -> Value {
    let (status, model) = http_request(&ctx.addr, "GET", "/api/model", &[], None);
    assert_eq!(status, 200, "{model}");
    assert_eq!(commandable(&model, "lamp"), json!(true), "granted light");
    assert_eq!(
        commandable(&model, "heat_pump"),
        json!(true),
        "granted climate"
    );
    assert_eq!(
        commandable(&model, "relay"),
        json!(false),
        "ungranted switch"
    );
    assert_eq!(
        commandable(&model, "family_member"),
        json!(false),
        "a tracker takes no commands"
    );
    let (status, reply) = http_request(
        &ctx.addr,
        "POST",
        "/api/cmd",
        &[("X-Homeostat", "family")],
        Some(&json!({"room": "livingroom", "entity": "relay", "aspect": "on", "value": true})),
    );
    assert_eq!(status, 400, "ungranted capability must be refused: {reply}");
    // An ungranted command does not reach the bus.
    expect_silence(&ctx.switch_cmd_sub, SILENCE, "switch command").await;
    model
}

/// A climate command is accepted: COMMANDABLE maps climate ->
/// {"setpoint"}, the family-facing base aspect
/// (docs/design.md#the-capability-vocabulary). Setpoint is a float, and
/// the envelope must carry it through unmodified like any other value.
async fn climate_takes_only_its_setpoint(ctx: &Ctx) {
    let (status, reply) = http_request(
        &ctx.addr,
        "POST",
        "/api/cmd",
        &[("X-Homeostat", "family")],
        Some(
            &json!({"room": "livingroom", "entity": "heat_pump", "aspect": "setpoint", "value": 21.5}),
        ),
    );
    assert_eq!(status, 200, "{reply}");
    let envelope = next_sample(&ctx.climate_cmd_sub, Duration::from_secs(30))
        .await
        .expect("climate cmd envelope observed within 10s")
        .value()
        .clone();
    let cmd_id = envelope["id"]
        .as_str()
        .expect("envelope carries a correlation id");
    assert_eq!(
        envelope,
        json!({"value": 21.5, "priority": "manual", "actor": "dashboard", "id": cmd_id}),
        "climate setpoint command stamps the same manual-band envelope, float value intact"
    );

    // Every other climate aspect is read-only passthrough: not commandable.
    let (status, reply) = http_request(
        &ctx.addr,
        "POST",
        "/api/cmd",
        &[("X-Homeostat", "family")],
        Some(
            &json!({"room": "livingroom", "entity": "heat_pump", "aspect": "feed_temperature_target", "value": 45.0}),
        ),
    );
    assert_eq!(
        status, 400,
        "non-commandable climate aspect must be refused: {reply}"
    );
}

/// A notifier takes its text: `message` is the base aspect and `alert`
/// the feature this phone declares. Each is published as it was typed,
/// in the same manual-band envelope.
async fn notifier_takes_its_text(ctx: &Ctx, model: &Value) {
    assert_eq!(commandable(model, "phone"), json!(true), "granted notifier");
    for aspect in ["message", "alert"] {
        let (status, reply) = http_request(
            &ctx.addr,
            "POST",
            "/api/cmd",
            &[("X-Homeostat", "family")],
            Some(
                &json!({"room": "person", "entity": "phone", "aspect": aspect, "value": "Dinner is ready"}),
            ),
        );
        assert_eq!(status, 200, "{aspect}: {reply}");
        let cmd = next_sample(&ctx.notify_cmd_sub, Duration::from_secs(30))
            .await
            .expect("notifier cmd envelope observed");
        assert_eq!(cmd.key, format!("home/cmd/person/phone/{aspect}"));
        assert_eq!(
            cmd.value(),
            &json!({"value": "Dinner is ready", "priority": "manual", "actor": "dashboard", "id": reply["id"]}),
        );
    }
    // Text that is blank, too long or not a string does not reach the bus.
    for value in [json!("  "), json!("x".repeat(1001)), json!(42)] {
        let (status, reply) = http_request(
            &ctx.addr,
            "POST",
            "/api/cmd",
            &[("X-Homeostat", "family")],
            Some(
                &json!({"room": "person", "entity": "phone", "aspect": "message", "value": value}),
            ),
        );
        assert_eq!(status, 400, "{value}: {reply}");
    }
    // Refused text does not reach the bus.
    expect_silence(&ctx.notify_cmd_sub, SILENCE, "notifier command").await;
}

/// 2b. Aspect descriptors (docs/design.md#aspect-descriptors): the
/// owning adapter's discovery record may describe an entity's aspects
/// and declare commands beyond the capability's vocabulary. A
/// family-editable one is admitted at /api/cmd within its constraint;
/// an owner-tier one stays refused; and the descriptor rides the
/// WebSocket snapshot so the page can render it.
async fn aspect_descriptors_admit_family_commands(ctx: &Ctx) {
    ctx.observer
        .put(
            "home/discovery/reflector",
            json!([{
                "id": "heat_pump-1", "configured": true, "entity": "heat_pump", "bound": true,
                "suggested": {"capability": "climate", "features": []},
                "aspects": {
                    "schema": 1,
                    "groups": ["control"],
                    "fields": {
                        "operating_mode": {
                            "label": "mode", "kind": "enum", "group": "control",
                            "values": [{"value": 1, "label": "normal"}, {"value": 2, "label": "block"}],
                            "command": {"type": "enum", "editable_by": "family"}
                        },
                        "feed_temperature_target": {
                            "label": "feed target", "kind": "temperature", "group": "control",
                            "command": {"type": "float", "constraint": {"min": 20, "max": 60}, "editable_by": "owner"}
                        }
                    }
                }
            }])
            .to_string(),
        )
        .await
        .expect("discovery publish");
    let deadline = Instant::now() + Duration::from_secs(10);
    let snapshot = loop {
        let mut ws = ws_connect(&ctx.addr, "/ws");
        let snapshot = ws_read_message(&mut ws);
        if snapshot["aspects"]["heat_pump"]["fields"]["operating_mode"]["label"] == json!("mode") {
            break snapshot;
        }
        assert!(
            Instant::now() < deadline,
            "snapshot never carried the descriptor: {snapshot}"
        );
        std::thread::sleep(Duration::from_millis(200));
    };
    assert_eq!(
        snapshot["aspects"]["heat_pump"]["groups"],
        json!(["control"])
    );
    let (status, reply) = http_request(
        &ctx.addr,
        "POST",
        "/api/cmd",
        &[("X-Homeostat", "family")],
        Some(
            &json!({"room": "livingroom", "entity": "heat_pump", "aspect": "operating_mode", "value": 2}),
        ),
    );
    assert_eq!(status, 200, "descriptor-declared family command: {reply}");
    // The wildcard subscriber also saw the earlier setpoint command; the
    // descriptor command is the next envelope on a different key.
    let cmd = await_sample(
        &ctx.mode_cmd_sub,
        Duration::from_secs(30),
        "descriptor command envelope",
        |msg| msg.key == "home/cmd/livingroom/heat_pump/operating_mode",
    )
    .await;
    let envelope = cmd.value().clone();
    let cmd_id = envelope["id"]
        .as_str()
        .expect("envelope carries a correlation id");
    assert_eq!(
        envelope,
        json!({"value": 2, "priority": "manual", "actor": "dashboard", "id": cmd_id})
    );
    let (status, reply) = http_request(
        &ctx.addr,
        "POST",
        "/api/cmd",
        &[("X-Homeostat", "family")],
        Some(
            &json!({"room": "livingroom", "entity": "heat_pump", "aspect": "operating_mode", "value": 4}),
        ),
    );
    assert_eq!(
        status, 400,
        "a value outside the declared enum is refused: {reply}"
    );
    let (status, reply) = http_request(
        &ctx.addr,
        "POST",
        "/api/cmd",
        &[("X-Homeostat", "family")],
        Some(
            &json!({"room": "livingroom", "entity": "heat_pump", "aspect": "feed_temperature_target", "value": 45.0}),
        ),
    );
    assert_eq!(
        status, 400,
        "an owner-tier descriptor command stays refused: {reply}"
    );
    // Refused descriptor commands never reach the bus.
    expect_silence(&ctx.mode_cmd_sub, SILENCE, "heat pump command").await;
}

/// 3. Parameter path: in-constraint persists, out-of-constraint refused.
async fn params_persist_within_constraints(ctx: &Ctx) {
    let (status, reply) = http_request(
        &ctx.addr,
        "POST",
        "/api/param",
        &[("X-Homeostat", "family")],
        Some(&json!({"unit": "evening_lights", "param": "off_time", "value": "21:30"})),
    );
    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply["value"], json!("21:30"));
    assert_eq!(
        cache_read(&ctx.observer, OFF_TIME_KEY).await,
        Some(json!("21:30"))
    );

    let (status, reply) = http_request(
        &ctx.addr,
        "POST",
        "/api/param",
        &[("X-Homeostat", "family")],
        Some(&json!({"unit": "evening_lights", "param": "off_time", "value": "03:00"})),
    );
    assert_eq!(status, 400, "out-of-constraint accepted: {reply}");
    assert!(reply["error"].is_string(), "{reply}");
    assert_eq!(
        cache_read(&ctx.observer, OFF_TIME_KEY).await,
        Some(json!("21:30")),
        "refused write must change nothing"
    );

    // An owner param is visible but not writable from the family surface.
    let (status, reply) = http_request(
        &ctx.addr,
        "POST",
        "/api/param",
        &[("X-Homeostat", "family")],
        Some(&json!({"unit": "evening_lights", "param": "grace_minutes", "value": 10})),
    );
    assert_eq!(status, 400, "owner param write must be refused: {reply}");
    assert_eq!(
        cache_read(&ctx.observer, "home/config/evening_lights/grace_minutes").await,
        Some(json!(5)),
        "refused write must change nothing"
    );
}

/// 4. The local-only gate.
fn local_only_gate_holds(ctx: &Ctx) {
    let (status, _) = http_request(
        &ctx.addr,
        "POST",
        "/api/param",
        &[],
        Some(&json!({"unit": "evening_lights", "param": "off_time", "value": "21:00"})),
    );
    assert_eq!(status, 403, "write without X-Homeostat must be refused");
    // The Host rule is the MCP server's too: one table pins both.
    let table: Value = serde_json::from_str(include_str!("../fixtures/host_gate.json"))
        .expect("host_gate.json is JSON");
    for (list, want) in [("allowed", 200), ("refused", 403)] {
        for host in table[list].as_array().expect("a list of hosts") {
            let host = host.as_str().expect("a host string");
            let (status, _) = http_request(&ctx.addr, "GET", "/", &[("Host", host)], None);
            assert_eq!(status, want, "Host {host:?}");
        }
    }
}

/// 9. `/api/history` builds a recorder selector from browser input, so
///    the entity must be one the model knows and the aspect one key
///    segment. A wildcard would fan the per-series limit out over the
///    whole store, and `/`, `#`, `$` would raise inside the executor.
///    No recorder in this fixture: a valid pair answers an empty series
///    list, which is the 200 that matters here.
fn history_checks_its_selector(ctx: &Ctx) {
    let (status, reply) = http_request(
        &ctx.addr,
        "GET",
        "/api/history?entity=lamp&aspect=brightness",
        &[],
        None,
    );
    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply["series"], json!([]), "{reply}");
    // The recorder's chart shapes pass through, one at a time.
    for query in [
        "entity=lamp&aspect=brightness&hours=168&bucket=1512",
        "entity=lamp&aspect=on&changes=1",
    ] {
        let (status, reply) = http_request(
            &ctx.addr,
            "GET",
            &format!("/api/history?{query}"),
            &[],
            None,
        );
        assert_eq!(status, 200, "{query}: {reply}");
    }
    for query in [
        "entity=**&aspect=brightness",
        "entity=lamp&aspect=**",
        "entity=lamp&aspect=bright/ness",
        "entity=lamp&aspect=$foo",
        "entity=lamp&aspect=..",
        "entity=no_such&aspect=brightness",
        "entity=lamp&aspect=brightness&bucket=soon",
        "entity=lamp&aspect=brightness&bucket=-5",
        "entity=lamp&aspect=brightness&hours=744&bucket=1",
        "entity=lamp&aspect=brightness&bucket=60&changes=1",
        // A bucket too large for a float.
        &format!("entity=lamp&aspect=brightness&bucket=1{}", "0".repeat(400)),
    ] {
        let (status, reply) = http_request(
            &ctx.addr,
            "GET",
            &format!("/api/history?{query}"),
            &[],
            None,
        );
        assert_eq!(status, 400, "{query}: {reply}");
    }
}

/// 10. The recorder routes read `hours` as a window length. One that is
///     not a finite positive number is a 400 on each of them.
fn windows_refuse_bad_hours(ctx: &Ctx) {
    for path in [
        "/api/history?entity=lamp&aspect=brightness",
        "/api/forecasts?entity=lamp&aspect=brightness",
        "/api/source-events?entity=lamp&aspect=brightness",
        "/api/unit-events?unit=evening_lights",
        "/api/unit-timeline?unit=evening_lights",
    ] {
        let (status, reply) =
            http_request(&ctx.addr, "GET", &format!("{path}&hours=24"), &[], None);
        assert_eq!(status, 200, "{path}: {reply}");
        for hours in ["soon", "nan", "inf", "-inf", "0", "-1", "-1e300"] {
            let (status, reply) = http_request(
                &ctx.addr,
                "GET",
                &format!("{path}&hours={hours}"),
                &[],
                None,
            );
            assert_eq!(status, 400, "{path} hours={hours}: {reply}");
        }
    }
}

/// The `commandable` flag the model gives the entity called `name`.
fn commandable(model: &Value, name: &str) -> Value {
    model["entities"]
        .as_array()
        .expect("entities")
        .iter()
        .find(|e| e["name"] == name)
        .unwrap_or_else(|| panic!("{name} in model"))["commandable"]
        .clone()
}

/// 5. With `HOMEOSTAT_DASHBOARD_TILES` set, /tiles.pmtiles serves the file
/// in full and as a Range-respecting partial response, and /api/model
/// says so.
#[tokio::test(flavor = "multi_thread")]
async fn dashboard_serves_configured_tiles() {
    let port = common::free_port();
    let addr = format!("127.0.0.1:{port}");
    let scratch = common::Scratch::new("dashboard-tiles");
    let tiles_path = scratch.join("tiles.pmtiles");
    let contents = b"fake-pmtiles-bytes-0123456789";
    std::fs::write(&tiles_path, contents).expect("write fixture tiles file");

    let (mut sup, _observer) = up(
        || {
            Supervisor::spawn_with_env(
                FIXTURE,
                &[
                    ("HOMEOSTAT_DASHBOARD_PORT", &port.to_string()),
                    (
                        "HOMEOSTAT_DASHBOARD_TILES",
                        tiles_path.to_str().expect("utf-8 path"),
                    ),
                ],
            )
        },
        &["dashboard"],
    )
    .await;

    let (status, model) = http_request(&addr, "GET", "/api/model", &[], None);
    assert_eq!(status, 200, "{model}");
    assert_eq!(model["tiles"], json!(true), "{model}");

    let HttpResponse {
        status,
        headers,
        body,
    } = http(&addr, "GET", "/tiles.pmtiles", &[], None);
    assert_eq!(status, 200);
    assert_eq!(body, contents);
    assert!(
        headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("accept-ranges") && v == "bytes"),
        "{headers:?}"
    );

    let HttpResponse {
        status,
        headers,
        body,
    } = http(
        &addr,
        "GET",
        "/tiles.pmtiles",
        &[("Range", "bytes=0-4")],
        None,
    );
    assert_eq!(status, 206, "{headers:?}");
    assert_eq!(body, &contents[..5]);
    assert!(
        headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("content-range")),
        "{headers:?}"
    );

    sup.shutdown();
}

/// 6. `/api/logs` proxies the supervisor's `home/meta/{unit}/log` queryable:
/// the `logger` fixture unit (`fake_adapter` --print-stdout/--print-stderr)
/// prints known lines at startup, stream-tagged and truncatable via
/// `lines=N`; an unknown unit 404s.
#[tokio::test(flavor = "multi_thread")]
async fn dashboard_serves_unit_logs() {
    let port = common::free_port();
    let addr = format!("127.0.0.1:{port}");
    let (mut sup, observer) = up(
        || Supervisor::spawn_with_env(FIXTURE, &[("HOMEOSTAT_DASHBOARD_PORT", &port.to_string())]),
        &["dashboard", "logger"],
    )
    .await;

    // Wait for the logger's known startup lines to land in its ring buffer.
    eventually(Duration::from_secs(10), "logger startup lines", || async {
        let lines = cache_read(&observer, &bus::log_key("logger")).await?;
        (lines.as_array()?.len() >= 8).then_some(())
    })
    .await;

    let (status, entries) = http_request(&addr, "GET", "/api/logs?unit=logger", &[], None);
    assert_eq!(status, 200, "{entries}");
    let rows = entries.as_array().expect("array body");
    assert_eq!(rows.len(), 8, "{entries}");
    assert!(
        rows.iter()
            .any(|e| e["stream"] == "stdout" && e["line"] == "stdout-line-0"),
        "{entries}"
    );
    assert!(
        rows.iter()
            .any(|e| e["stream"] == "stderr" && e["line"] == "stderr-line-2"),
        "{entries}"
    );
    assert!(rows[0]["ts_us"].is_number(), "{entries}");

    let (status, entries) = http_request(&addr, "GET", "/api/logs?unit=logger&lines=2", &[], None);
    assert_eq!(status, 200, "{entries}");
    assert_eq!(
        entries.as_array().expect("array body").len(),
        2,
        "lines=2 truncates to the tail: {entries}"
    );

    let (status, reply) = http_request(&addr, "GET", "/api/logs?unit=no-such-unit", &[], None);
    assert_eq!(status, 404, "unknown unit must 404: {reply}");

    sup.shutdown();
}

/// `/api/unit-events` reads a unit's health events from the recorder, so
/// the unit overlay keeps them across a reload. The test session answers
/// `home/history/events` in the recorder's place, which pins the selector
/// the dashboard sends and the shape it returns.
#[tokio::test(flavor = "multi_thread")]
async fn dashboard_serves_a_units_recorded_events() {
    let port = common::free_port();
    let addr = format!("127.0.0.1:{port}");
    let (mut sup, observer) = up(
        || Supervisor::spawn_with_env(FIXTURE, &[("HOMEOSTAT_DASHBOARD_PORT", &port.to_string())]),
        &["dashboard"],
    )
    .await;

    let queryable = observer
        .declare_queryable("home/history/events")
        .await
        .expect("stand-in recorder");
    let (selectors, mut asked) = tokio::sync::mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        while let Ok(query) = queryable.recv_async().await {
            let _ = selectors.send(query.parameters().to_string());
            let rows = json!([
                {"ts": 1_700_000_000_000_000_u64, "key": "home/health/evening_lights/event",
                 "payload": {"kind": "paused"}},
                {"ts": 1_700_000_060_500_000_u64, "key": "home/health/evening_lights/event",
                 "payload": {"kind": "resumed"}},
                // Not JSON on the wire: the recorder keeps the raw text.
                {"ts": 1_700_000_070_000_000_u64, "key": "home/health/evening_lights/event",
                 "payload": "not json"},
            ]);
            let _ = query.reply("home/history/events", rows.to_string()).await;
        }
    });

    // The stand-in's queryable reaches the dashboard's session after a
    // moment. Until then the get finds no recorder and answers no events.
    let deadline = Instant::now() + Duration::from_secs(10);
    let reply = loop {
        let (status, reply) = http_request(
            &addr,
            "GET",
            "/api/unit-events?unit=evening_lights",
            &[],
            None,
        );
        assert_eq!(status, 200, "{reply}");
        if reply["events"].as_array().is_some_and(|e| !e.is_empty()) {
            break reply;
        }
        assert!(
            Instant::now() < deadline,
            "the stand-in recorder was never asked"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(
        reply["events"],
        json!([
            {"ts": 1_700_000_000.0, "key": "home/health/evening_lights/event", "value": {"kind": "paused"}},
            {"ts": 1_700_000_060.5, "key": "home/health/evening_lights/event", "value": {"kind": "resumed"}},
        ]),
        "ts in seconds, the shape of a live event, and a raw text row skipped"
    );
    let selector = asked.recv().await.expect("a selector");
    assert!(
        selector.contains("key=home/health/evening_lights/event"),
        "only the unit's own events: {selector}"
    );
    assert!(selector.contains("limit=200"), "{selector}");

    let (status, reply) = http_request(&addr, "GET", "/api/unit-events?unit=nobody", &[], None);
    assert_eq!(status, 404, "{reply}");
    for query in ["hours=soon", "hours=0", "hours=-1"] {
        let (status, reply) = http_request(
            &addr,
            "GET",
            &format!("/api/unit-events?unit=evening_lights&{query}"),
            &[],
            None,
        );
        assert_eq!(status, 400, "{query}: {reply}");
    }

    sup.shutdown();
}

/// `/api/unit-timeline` gathers an automation's rows from the recorder:
/// its own commands among the house's, the events that answered them, its
/// parameter writes, its health events and how the commanded aspects
/// changed. The test session answers in the recorder's place.
#[tokio::test(flavor = "multi_thread")]
async fn dashboard_gathers_an_automations_timeline() {
    const T: u64 = 1_700_000_000_000_000;
    let port = common::free_port();
    let addr = format!("127.0.0.1:{port}");
    let (mut sup, observer) = up(
        || Supervisor::spawn_with_env(FIXTURE, &[("HOMEOSTAT_DASHBOARD_PORT", &port.to_string())]),
        &["dashboard"],
    )
    .await;

    let events = observer
        .declare_queryable("home/history/events")
        .await
        .expect("stand-in recorder events");
    tokio::spawn(async move {
        while let Ok(query) = events.recv_async().await {
            let params = query.parameters().to_string();
            let rows = if params.contains("key=home/cmd/**") {
                json!([
                    {"ts": T, "key": "home/cmd/livingroom/lamp/on",
                     "payload": {"value": false, "priority": "automation", "actor": "evening_lights", "id": "c1"}},
                    {"ts": T + 1, "key": "home/cmd/livingroom/lamp/on",
                     "payload": {"value": true, "priority": "manual", "actor": "dashboard", "id": "c2"}},
                ])
            } else if params.contains("key=home/health/*/event") {
                // A busy house: the reply fills the dashboard's cap of
                // 1000, so commands older than its oldest row are not
                // judged. A row that is not an object and a cmd_id that
                // is not a string are skipped.
                let mut rows: Vec<Value> = (0..996)
                    .map(|i| {
                        json!({"ts": T - 1000 + i, "key": "home/health/zigbee/event",
                                    "payload": {"kind": "drop", "cmd_id": "other"}})
                    })
                    .collect();
                rows.extend([
                    json!({"ts": T + 1, "key": "home/health/arbiter/event", "payload": "not an object"}),
                    json!({"ts": T + 1, "key": "home/health/arbiter/event",
                           "payload": {"kind": "refuse", "cmd_id": ["c1"]}}),
                    json!({"ts": T + 2, "key": "home/health/arbiter/event",
                           "payload": {"kind": "refuse", "cmd_id": "c1", "holder_actor": "dashboard"}}),
                    json!({"ts": T + 3, "key": "home/health/arbiter/event",
                           "payload": {"kind": "refuse", "cmd_id": "c9"}}),
                ]);
                Value::Array(rows)
            } else if params.contains("key=home/health/evening_lights/event") {
                json!([
                    {"ts": T + 4, "key": "home/health/evening_lights/event", "payload": {"kind": "paused"}},
                    {"ts": T + 4, "key": "home/health/evening_lights/event", "payload": "raw text"},
                ])
            } else if params.contains("key=home/config/evening_lights/*") {
                json!([{"ts": T + 5, "key": "home/config/evening_lights/off_time", "payload": "22:30"}])
            } else {
                json!([])
            };
            let _ = query.reply("home/history/events", rows.to_string()).await;
        }
    });
    let samples = observer
        .declare_queryable("home/history/state/**")
        .await
        .expect("stand-in recorder samples");
    tokio::spawn(async move {
        while let Ok(query) = samples.recv_async().await {
            let key = query.key_expr().as_str().to_string();
            let points =
                json!([{"ts": "2023-11-14T22:13:21+00:00", "room": "livingroom", "value": false}]);
            let _ = query.reply(key, points.to_string()).await;
        }
    });

    let deadline = Instant::now() + Duration::from_secs(10);
    let reply = loop {
        let (status, reply) = http_request(
            &addr,
            "GET",
            "/api/unit-timeline?unit=evening_lights",
            &[],
            None,
        );
        assert_eq!(status, 200, "{reply}");
        if reply["commands"].as_array().is_some_and(|c| !c.is_empty()) {
            break reply;
        }
        assert!(
            Instant::now() < deadline,
            "the stand-in recorder was never asked"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let ids: Vec<&Value> = reply["commands"]
        .as_array()
        .expect("commands")
        .iter()
        .map(|c| &c["value"]["id"])
        .collect();
    assert_eq!(ids, vec![&json!("c1")], "only its own commands: {reply}");
    let answers = reply["answers"].as_array().expect("answers");
    assert_eq!(answers.len(), 1, "only events naming its commands: {reply}");
    assert_eq!(answers[0]["value"]["cmd_id"], "c1");
    assert_eq!(
        reply["events"],
        json!([{"ts": 1_700_000_000.000_004, "key": "home/health/evening_lights/event",
                "value": {"kind": "paused"}}]),
        "only object rows: {reply}"
    );
    assert_eq!(reply["params"][0]["value"], "22:30", "{reply}");
    let readback = &reply["readbacks"][0];
    assert_eq!(
        (&readback["entity"], &readback["aspect"]),
        (&json!("lamp"), &json!("on"))
    );
    assert_eq!(
        readback["points"][0]["ts"],
        json!(1_700_000_001.0),
        "ts in seconds: {reply}"
    );
    assert_eq!(readback["from"], Value::Null, "a series under its cap");
    assert_eq!(
        reply["answers_from"],
        json!((T - 1000) as f64 / 1e6),
        "the oldest answer row kept: {reply}"
    );
    assert_eq!(reply["truncated"], true, "{reply}");
    assert!(
        reply.get("inputs").is_none(),
        "the aspects it reads are not gathered: {reply}"
    );

    let (status, reply) = http_request(&addr, "GET", "/api/unit-timeline?unit=nobody", &[], None);
    assert_eq!(status, 404, "{reply}");
    let (status, reply) = http_request(
        &addr,
        "GET",
        "/api/unit-timeline?unit=evening_lights&hours=0",
        &[],
        None,
    );
    assert_eq!(status, 400, "{reply}");

    sup.shutdown();
}

/// Sends one masked client text frame (an all-zero mask key is valid RFC
/// 6455 masking, and XOR with zero leaves the payload readable).
fn ws_send_text(stream: &mut TcpStream, payload: &str) {
    let bytes = payload.as_bytes();
    assert!(bytes.len() < 126, "small frames only in tests");
    let mut frame = vec![0x81u8, 0x80 | bytes.len() as u8, 0, 0, 0, 0];
    frame.extend_from_slice(bytes);
    stream.write_all(&frame).expect("send ws frame");
}

/// Reads one server frame, returning (opcode, payload).
fn ws_read_frame(stream: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut prefix = [0u8; 2];
    stream.read_exact(&mut prefix).expect("frame header");
    let mut len = u64::from(prefix[1] & 0x7f);
    if len == 126 {
        let mut ext = [0u8; 2];
        stream.read_exact(&mut ext).expect("extended len");
        len = u64::from(u16::from_be_bytes(ext));
    } else if len == 127 {
        let mut ext = [0u8; 8];
        stream.read_exact(&mut ext).expect("extended len");
        len = u64::from_be_bytes(ext);
    }
    let mut payload = vec![0u8; len as usize];
    stream.read_exact(&mut payload).expect("frame payload");
    (prefix[0] & 0x0f, payload)
}

#[tokio::test(flavor = "multi_thread")]
async fn dashboard_proxies_camera_media() {
    // The fake serves only the stream go2rtc would have: named by
    // the entity's id, which the fixture keeps distinct from its name.
    let go2rtc = FakeServer::spawn("tests/fake_go2rtc.py", "--streams porch_cam_native").await;
    let port = common::free_port();
    let addr = format!("127.0.0.1:{port}");
    // The dashboard proxies reach it through `HOMEOSTAT_GO2RTC`, as they
    // would reach the shim-supervised real one on localhost.
    let go2rtc_url = format!("http://127.0.0.1:{}", go2rtc.port);
    let (mut sup, _observer) = up(
        || {
            Supervisor::spawn_with_env(
                FIXTURE,
                &[
                    ("HOMEOSTAT_DASHBOARD_PORT", &port.to_string()),
                    ("HOMEOSTAT_GO2RTC", &go2rtc_url),
                ],
            )
        },
        &["dashboard"],
    )
    .await;

    // The camera renders in the model like any entity.
    let (status, model) = http_request(&addr, "GET", "/api/model", &[], None);
    assert_eq!(status, 200, "{model}");
    let cam = model["entities"]
        .as_array()
        .expect("entities array")
        .iter()
        .find(|e| e["name"] == "porch_cam")
        .expect("porch_cam in model");
    assert_eq!(cam["capability"], "camera", "{cam}");
    assert!(
        cam.get("id").is_none(),
        "the adapter-native id stays server-side, out of the browser model: {cam}"
    );

    // No snapshot route: go2rtc can only make a JPEG of an H.264 source by
    // transcoding, and the media plane only remuxes, so the room card says
    // "tap to view" rather than waiting on a frame that would not arrive.
    let (status, _) = http_request(&addr, "GET", "/api/camera/porch_cam/snapshot", &[], None);
    assert_eq!(status, 404, "there is no snapshot proxy");

    // Unknown and non-camera entities 404, because the live proxy is
    // model-gated.
    let (status, _) = http_request(&addr, "GET", "/api/camera/no_such_cam/live", &[], None);
    assert_eq!(status, 404);
    let (status, _) = http_request(&addr, "GET", "/api/camera/lamp/live", &[], None);
    assert_eq!(status, 404, "a lamp is not a camera");

    // Live: the MSE handshake and the fMP4 bytes relay untouched.
    let mut ws = ws_connect(&addr, "/api/camera/porch_cam/live");
    ws_send_text(&mut ws, "{\"type\":\"mse\"}");
    let (opcode, payload) = ws_read_frame(&mut ws);
    assert_eq!(opcode, 0x1, "codec answer is a text frame");
    let answer: Value = serde_json::from_slice(&payload).expect("codec answer is JSON");
    assert_eq!(answer["type"], "mse", "{answer}");
    let (opcode, init) = ws_read_frame(&mut ws);
    assert_eq!(opcode, 0x2, "init segment is a binary frame");
    assert_eq!(init, b"\x00\x00\x00\x1cftypiso5FAKEMOOV".to_vec());
    let (opcode, frame) = ws_read_frame(&mut ws);
    assert_eq!(opcode, 0x2, "media segment is a binary frame");
    assert_eq!(frame, b"\x00\x00\x00\x10moofFAKEFRAME".to_vec());

    sup.shutdown();
}

/// 8. The whole-house darken (docs/design.md#commanding, "Group actions fan
/// out at the manual edge"). POST /api/lights/off publishes one
/// manual-band off-command per bound light, and nothing for other
/// entities, behind the same write gate as every other command.
#[tokio::test(flavor = "multi_thread")]
async fn dashboard_darkens_the_house() {
    let port = common::free_port();
    let addr = format!("127.0.0.1:{port}");
    let permit = common::startup_permit().await;
    let mut sup =
        Supervisor::spawn_with_env(FIXTURE, &[("HOMEOSTAT_DASHBOARD_PORT", &port.to_string())]);
    let observer = sup.observer().await;

    // Declared before the health wait: the startup window is the settle
    // time for subscriber-interest propagation (see the family surface
    // test's note).
    let cmd_sub = observer
        .declare_subscriber("home/cmd/**")
        .await
        .expect("cmd subscriber");

    await_running(&observer, "dashboard", FIRST_RUN).await;
    drop(permit);

    // The write gate holds for the fan-out like any other write.
    let (status, reply) = http_request(&addr, "POST", "/api/lights/off", &[], None);
    assert_eq!(status, 403, "{reply}");

    let (status, reply) = http_request(
        &addr,
        "POST",
        "/api/lights/off",
        &[("X-Homeostat", "family")],
        None,
    );
    assert_eq!(status, 200, "{reply}");
    assert_eq!(
        reply,
        json!({"ok": true, "lights": 1}),
        "the fixture binds exactly one light"
    );

    let cmd = next_sample(&cmd_sub, Duration::from_secs(30))
        .await
        .expect("light cmd within 10s");
    assert_eq!(cmd.key, LAMP_CMD);
    let envelope = cmd.value().clone();
    let cmd_id = envelope["id"]
        .as_str()
        .expect("envelope carries a correlation id");
    assert_eq!(
        envelope,
        json!({"value": false, "priority": "manual", "actor": "dashboard", "id": cmd_id}),
        "the fan-out is stamped like any other dashboard command"
    );

    // Only lights: the switch, lock, climate and camera entities get
    // nothing. The session delivers in order, so anything else the fan-out
    // had published would arrive before this window closes.
    expect_silence(&cmd_sub, SILENCE, "other command").await;

    sup.shutdown();
}
