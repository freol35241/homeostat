//! ESPHome adapter integration tests: each scenario spawns a real fake
//! ESPHome device (tests/fake_esphome.py, the real plaintext wire protocol
//! over aioesphomeapi's bundled protobuf messages) on a free port plus the
//! real supervisor on the esphome fixture house, and asserts on both buses.

mod common;

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use homeostat::bus::HealthStatus;
use serde_json::{json, Value};
use zenoh::handlers::FifoChannelHandler;
use zenoh::pubsub::Subscriber;
use zenoh::sample::{Sample, SampleKind};

use common::{await_health, free_port, health_watch, process_alive, Supervisor};

const FIXTURE: &str = "tests/fixture_house_esphome";
const DEVICES_ENV: &str = "HOMEOSTAT_ESPHOME_DEVICES";
const EVENT_KEY: &str = "home/health/esphome/event";
const RELAY_STATE_KEY: &str = "home/state/shed/relay/on";
const RELAY_CMD_KEY: &str = "home/cmd/shed/relay/on";

/// A fake ESPHome device (tests/fake_esphome.py) on a free port, killed on
/// drop. Spawned the same way the units themselves are: `uv run`.
struct FakeEsphome {
    child: Child,
    port: u16,
}

impl FakeEsphome {
    fn spawn() -> Self {
        let port = free_port();
        let child = Command::new("uv")
            .args([
                "run",
                "tests/fake_esphome.py",
                "--port",
                &port.to_string(),
                "--name",
                "shed",
            ])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn fake esphome device (is uv installed?)");
        // First run resolves the fake device's own uv env: generous.
        let deadline = Instant::now() + Duration::from_secs(60);
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(Instant::now() < deadline, "fake esphome device never listened on {port}");
            std::thread::sleep(Duration::from_millis(50));
        }
        Self { child, port }
    }
}

impl Drop for FakeEsphome {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Writes a HOMEOSTAT_ESPHOME_DEVICES file (outside the repo, per the
/// settlement) giving the fixture's "shed" device a host override at the
/// fake device's port.
fn devices_file(port: u16) -> PathBuf {
    let path = std::env::temp_dir().join(format!("homeostat-esphome-devices-{port}.toml"));
    std::fs::write(&path, format!("[shed]\nhost = \"127.0.0.1:{port}\"\n"))
        .expect("write devices file");
    path
}

/// Spawns the fake device + supervisor on the fixture and waits for the
/// adapter's liveliness token (generous timeout: first run resolves the uv
/// env for aioesphomeapi/zeroconf too).
async fn setup() -> (FakeEsphome, PathBuf, Supervisor, zenoh::Session) {
    let device = FakeEsphome::spawn();
    let devices_path = devices_file(device.port);
    let sup = Supervisor::spawn_with_env(
        FIXTURE,
        &[(DEVICES_ENV, devices_path.to_str().expect("utf-8 path"))],
    );
    let observer = sup.observer().await;
    let token_sub = observer
        .liveliness()
        .declare_subscriber("home/health/esphome/alive")
        .history(true)
        .await
        .expect("liveliness subscriber");
    let token = tokio::time::timeout(Duration::from_secs(90), token_sub.recv_async())
        .await
        .expect("adapter liveliness token within 90s")
        .expect("liveliness stream open");
    assert_eq!(token.kind(), SampleKind::Put);
    (device, devices_path, sup, observer)
}

type StateSub = Subscriber<FifoChannelHandler<Sample>>;

/// Collects state samples until every `expected` (key, value) has appeared.
async fn expect_states(sub: &StateSub, expected: &[(&str, Value)]) {
    let mut seen: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while expected
        .iter()
        .any(|(key, value)| seen.get(*key) != Some(value))
    {
        let sample = tokio::time::timeout_at(deadline, sub.recv_async())
            .await
            .unwrap_or_else(|_| panic!("missing state keys; saw {seen:?}"))
            .expect("state stream open");
        let value: Value = serde_json::from_slice(&sample.payload().to_bytes())
            .expect("state payload is JSON");
        seen.insert(sample.key_expr().as_str().to_string(), value);
    }
}

/// Reads health events until one matches the expected drop reason.
async fn expect_drop_event(sub: &StateSub, reason: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let sample = tokio::time::timeout_at(deadline, sub.recv_async())
            .await
            .unwrap_or_else(|_| panic!("no \"{reason}\" health event within 20s"))
            .expect("event stream open");
        let event: Value = serde_json::from_slice(&sample.payload().to_bytes())
            .expect("health event is JSON");
        assert_eq!(event["kind"], "drop", "unexpected event kind: {event}");
        if event["reason"] == reason {
            return;
        }
    }
}

/// (a) The fake device's initial states translate to the correct home/state
/// keys: switch -> "on" (bool), sensor -> its device_class aspect, motion
/// binary_sensor -> presence capability's "occupancy" aspect.
#[tokio::test(flavor = "multi_thread")]
async fn device_state_translates_to_bus_state() {
    let (_device, _devices_path, mut sup, observer) = setup().await;
    let state_sub = observer.declare_subscriber("home/state/**").await.expect("state subscriber");

    expect_states(
        &state_sub,
        &[
            (RELAY_STATE_KEY, json!(false)),
            ("home/state/shed/shed_temp/temperature", json!(21.5)),
            ("home/state/shed/shed_motion/occupancy", json!(true)),
        ],
    )
    .await;

    sup.shutdown();
}

/// (b) A manual-band cmd envelope on the switch reaches the fake device as
/// a SwitchCommandRequest, which echoes the new state back — landing on
/// the bus as the same translated state key.
#[tokio::test(flavor = "multi_thread")]
async fn cmd_envelope_reaches_fake_device_and_echoes_back() {
    let (_device, _devices_path, mut sup, observer) = setup().await;
    let state_sub = observer.declare_subscriber(RELAY_STATE_KEY).await.expect("state subscriber");
    expect_states(&state_sub, &[(RELAY_STATE_KEY, json!(false))]).await;

    observer
        .put(
            RELAY_CMD_KEY,
            json!({"value": true, "priority": "manual", "actor": "test"}).to_string(),
        )
        .await
        .expect("cmd put");
    expect_states(&state_sub, &[(RELAY_STATE_KEY, json!(true))]).await;

    sup.shutdown();
}

/// (c) The cmd contract: a bare value with no envelope drops with an
/// "invalid-command" health event instead of reaching the device, and a
/// properly enveloped command afterwards still works.
#[tokio::test(flavor = "multi_thread")]
async fn envelope_less_command_drops_with_health_event() {
    let (_device, _devices_path, mut sup, observer) = setup().await;
    let event_sub = observer.declare_subscriber(EVENT_KEY).await.expect("event subscriber");
    let state_sub = observer.declare_subscriber(RELAY_STATE_KEY).await.expect("state subscriber");
    expect_states(&state_sub, &[(RELAY_STATE_KEY, json!(false))]).await;

    observer.put(RELAY_CMD_KEY, "true").await.expect("cmd put");
    expect_drop_event(&event_sub, "invalid-command").await;

    observer
        .put(
            RELAY_CMD_KEY,
            json!({"value": true, "priority": "manual", "actor": "test"}).to_string(),
        )
        .await
        .expect("cmd put");
    expect_states(&state_sub, &[(RELAY_STATE_KEY, json!(true))]).await;

    sup.shutdown();
}

/// (d) Discovery: every entity of the connected bound device lands in one
/// JSON document at home/discovery/esphome, each with a suggested
/// capability/features stanza and the raw ESPHome descriptor.
#[tokio::test(flavor = "multi_thread")]
async fn bound_device_entities_published_as_discovery() {
    let (_device, _devices_path, mut sup, observer) = setup().await;
    let sub = observer
        .declare_subscriber("home/discovery/esphome")
        .await
        .expect("discovery subscriber");

    let sample = tokio::time::timeout(Duration::from_secs(30), sub.recv_async())
        .await
        .expect("discovery document within 30s")
        .expect("discovery sample");
    let doc: Value =
        serde_json::from_slice(&sample.payload().to_bytes()).expect("discovery is JSON");
    let records = doc.as_array().expect("discovery is an array");
    assert_eq!(records.len(), 3, "{doc}");

    let relay = records
        .iter()
        .find(|r| r["id"] == json!("shed/relay"))
        .expect("relay record");
    assert_eq!(relay["configured"], json!(true));
    assert_eq!(relay["entity"], json!("relay"));
    assert_eq!(relay["suggested"]["capability"], json!("switch"));
    assert_eq!(relay["description"]["type"], json!("Switch"));

    let temp = records
        .iter()
        .find(|r| r["id"] == json!("shed/temperature"))
        .expect("temperature record");
    assert_eq!(temp["configured"], json!(true));
    assert_eq!(temp["entity"], json!("shed_temp"));
    assert_eq!(temp["suggested"]["capability"], json!("sensor"));
    assert_eq!(temp["description"]["device_class"], json!("temperature"));

    let motion = records
        .iter()
        .find(|r| r["id"] == json!("shed/motion"))
        .expect("motion record");
    assert_eq!(motion["configured"], json!(true));
    assert_eq!(motion["entity"], json!("shed_motion"));
    assert_eq!(motion["suggested"]["capability"], json!("presence"));
    assert_eq!(motion["description"]["device_class"], json!("motion"));

    // The mirror serves it to late joiners, like any discovery document.
    let replies = observer.get("home/discovery/esphome").await.expect("get discovery");
    let reply = replies.recv_async().await.expect("mirrored discovery reply");
    let mirrored: Value = serde_json::from_slice(
        &reply.result().expect("mirrored sample").payload().to_bytes(),
    )
    .expect("mirrored discovery is JSON");
    assert_eq!(mirrored, doc, "mirror serves the same document");

    sup.shutdown();
}

/// (e) The adapter honors the step-2 unit contract: liveliness token when
/// ready, clean SIGTERM shutdown within the grace, no orphans.
#[tokio::test(flavor = "multi_thread")]
async fn adapter_honors_unit_contract() {
    let (_device, _devices_path, mut sup, observer) = setup().await;
    let mut watch = health_watch(&observer, "esphome").await;
    let health = await_health(&mut watch, Duration::from_secs(10), |h| {
        h.status == HealthStatus::Running
    })
    .await;
    let adapter_pid = health.pid.expect("running unit has a pid");
    assert!(process_alive(adapter_pid), "adapter alive before shutdown");

    sup.signal(libc::SIGTERM);
    // shutdown_grace_s = 5 in the fixture; a graceful exit must fit inside
    // it with margin only for reaping and bus teardown.
    let code = sup.wait_exit(Duration::from_secs(7));
    assert_eq!(code, Some(0), "supervisor exit code");
    assert!(!process_alive(adapter_pid), "adapter must not outlive the supervisor");
}
