//! ONVIF adapter integration tests: each scenario spawns a fake ONVIF
//! pull-point event service (tests/fake_onvif.py — real SOAP shapes, real
//! WS-Security digest checking, a genuine long poll) on a free port plus
//! the real supervisor on the onvif fixture house, and asserts on the bus.

mod common;

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use zenoh::sample::SampleKind;

use common::{await_mirror, expect_drop_event, expect_state, free_port, StateSub, Supervisor};

const FIXTURE: &str = "tests/fixture_house_onvif";
const CAMERAS_ENV: &str = "HOMEOSTAT_CAMERAS";
const EVENT_KEY: &str = "home/health/onvif/event";
const MOTION_KEY: &str = "home/state/hallway/hallway_cam/motion";
const AVAILABLE_KEY: &str = "home/state/hallway/hallway_cam/available";
const USERNAME: &str = "homeostat";
const PASSWORD: &str = "secret123";

/// A fake ONVIF camera (tests/fake_onvif.py) on a free port, killed on
/// drop. Spawned the same way the units themselves are: `uv run`.
struct FakeOnvif {
    child: Child,
    port: u16,
}

impl FakeOnvif {
    fn spawn() -> Self {
        let port = free_port();
        let child = Command::new("uv")
            .args([
                "run",
                "tests/fake_onvif.py",
                "--port",
                &port.to_string(),
                "--username",
                USERNAME,
                "--password",
                PASSWORD,
            ])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn fake onvif camera (is uv installed?)");
        // First run resolves the fake camera's own uv env: generous.
        let deadline = Instant::now() + Duration::from_secs(60);
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(Instant::now() < deadline, "fake onvif camera never listened on {port}");
            std::thread::sleep(Duration::from_millis(50));
        }
        Self { child, port }
    }

    /// POSTs a control endpoint (plain HTTP, out of the SOAP path).
    fn control(&self, path: &str) {
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", self.port))
            .expect("connect to fake camera control");
        stream
            .write_all(
                format!(
                    "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .expect("write control request");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("read control response");
        assert!(response.starts_with("HTTP/1.1 200"), "control {path}: {response}");
    }
}

impl Drop for FakeOnvif {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Writes a HOMEOSTAT_CAMERAS file (outside the repo, per the settlement)
/// giving the fixture's "hallway_cam" the fake camera's host and the
/// camera-account credentials.
fn cameras_file(port: u16) -> PathBuf {
    let path = std::env::temp_dir().join(format!("homeostat-cameras-{port}.toml"));
    std::fs::write(
        &path,
        format!(
            "[hallway_cam]\nhost = \"127.0.0.1:{port}\"\nusername = \"{USERNAME}\"\npassword = \"{PASSWORD}\"\n"
        ),
    )
    .expect("write cameras file");
    path
}

/// Spawns the fake camera + supervisor on the fixture and waits for the
/// adapter's liveliness token (generous timeout: first run resolves the
/// uv env for aiohttp too).
async fn setup() -> (FakeOnvif, PathBuf, Supervisor, zenoh::Session) {
    let camera = FakeOnvif::spawn();
    let cameras_path = cameras_file(camera.port);
    let sup = Supervisor::spawn_with_env(
        FIXTURE,
        &[(CAMERAS_ENV, cameras_path.to_str().expect("utf-8 path"))],
    );
    let observer = sup.observer().await;
    let token_sub = observer
        .liveliness()
        .declare_subscriber("home/health/onvif/alive")
        .history(true)
        .await
        .expect("liveliness subscriber");
    let token = tokio::time::timeout(Duration::from_secs(90), token_sub.recv_async())
        .await
        .expect("adapter liveliness token within 90s")
        .expect("liveliness stream open");
    assert_eq!(token.kind(), SampleKind::Put);
    // Ready fires with the subscription attempt merely in flight; a trigger
    // before the pull-point subscription exists lands in zero queues and is
    // lost (a real camera's events during an outage are too). available =
    // true is the adapter's own signal that the subscription is up.
    await_mirror(&observer, AVAILABLE_KEY, &json!(true)).await;
    (camera, cameras_path, sup, observer)
}

/// Triggers repeatedly until the motion key carries `expected`. Triggers
/// are lossy by design: one fans out only to the subscriptions existing
/// at that instant, and the adapter abandons its subscription on any
/// fault (a trigger stranded in an abandoned queue is a real camera's
/// event during an outage) — so every test drives triggers through a
/// retry, never one-shot.
async fn trigger_until_motion(camera: &FakeOnvif, sub: &StateSub, expected: bool) {
    let value = if expected { "true" } else { "false" };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        camera.control(&format!("/control/trigger?value={value}"));
        let recv = tokio::time::timeout(Duration::from_secs(1), sub.recv_async()).await;
        if let Ok(Ok(sample)) = recv {
            let value: Value = serde_json::from_slice(&sample.payload().to_bytes())
                .expect("state payload is JSON");
            if value == json!(expected) {
                return;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no motion = {expected} within 30s of re-triggering"
        );
    }
}

/// Like `trigger_until_motion`, for trigger values whose observable
/// effect is a health event rather than state.
async fn trigger_until_event(camera: &FakeOnvif, sub: &StateSub, value: &str, reason: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        camera.control(&format!("/control/trigger?value={value}"));
        let recv = tokio::time::timeout(Duration::from_secs(1), sub.recv_async()).await;
        if let Ok(Ok(sample)) = recv {
            let event: Value = serde_json::from_slice(&sample.payload().to_bytes())
                .expect("health event is JSON");
            if event["reason"] == reason {
                return;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no \"{reason}\" health event within 30s of re-triggering"
        );
    }
}

/// (a) On-camera motion events translate to the camera entity's `motion`
/// aspect — the event plane in one assertion: pixels stay off the bus,
/// detections ride it as ordinary scalar state.
#[tokio::test(flavor = "multi_thread")]
async fn motion_events_translate_to_bus_state() {
    let (camera, _cameras_path, mut sup, observer) = setup().await;
    let state_sub = observer.declare_subscriber(MOTION_KEY).await.expect("state subscriber");

    trigger_until_motion(&camera, &state_sub, true).await;
    trigger_until_motion(&camera, &state_sub, false).await;

    sup.shutdown();
}

/// (b) The Tapo-regression contract: a broken subscription (every pull
/// faults) emits one "event-stream-lost" health event and the adapter
/// resubscribes from scratch — events flow again without a restart.
#[tokio::test(flavor = "multi_thread")]
async fn broken_subscription_resubscribes() {
    let (camera, _cameras_path, mut sup, observer) = setup().await;
    let state_sub = observer.declare_subscriber(MOTION_KEY).await.expect("state subscriber");
    let event_sub = observer.declare_subscriber(EVENT_KEY).await.expect("event subscriber");

    trigger_until_motion(&camera, &state_sub, true).await;

    camera.control("/control/break");
    expect_drop_event(&event_sub, "event-stream-lost").await;
    trigger_until_motion(&camera, &state_sub, false).await;

    sup.shutdown();
}

/// (b2) Availability rides the subscription: losing it publishes
/// available = false alongside the health event, and the recreated
/// subscription publishes available = true again — while `motion` stands
/// untouched through the outage (stale, never false).
#[tokio::test(flavor = "multi_thread")]
async fn subscription_loss_flips_available() {
    let (camera, _cameras_path, mut sup, observer) = setup().await;
    let avail_sub = observer.declare_subscriber(AVAILABLE_KEY).await.expect("available subscriber");
    let state_sub = observer.declare_subscriber(MOTION_KEY).await.expect("state subscriber");

    trigger_until_motion(&camera, &state_sub, true).await;

    camera.control("/control/break");
    expect_state(&avail_sub, json!(false)).await;
    expect_state(&avail_sub, json!(true)).await;

    sup.shutdown();
}

/// (c) A notification that parses but carries an unusable value drops
/// with a "malformed-payload" health event — and the stream survives it.
#[tokio::test(flavor = "multi_thread")]
async fn malformed_motion_value_drops_with_health_event() {
    let (camera, _cameras_path, mut sup, observer) = setup().await;
    let state_sub = observer.declare_subscriber(MOTION_KEY).await.expect("state subscriber");
    let event_sub = observer.declare_subscriber(EVENT_KEY).await.expect("event subscriber");

    trigger_until_event(&camera, &event_sub, "banana", "malformed-payload").await;

    trigger_until_motion(&camera, &state_sub, true).await;

    sup.shutdown();
}
