//! ESPHome adapter integration tests: each scenario spawns a real fake
//! ESPHome device (tests/fake_esphome.py, the real plaintext wire protocol
//! over aioesphomeapi's bundled protobuf messages) on a free port plus the
//! real supervisor on the esphome fixture house, and asserts on both buses.

mod common;

use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::json;
use zenoh::sample::SampleKind;

use common::{
    assert_unit_contract, await_discovery, await_mirror, await_states, expect_drop_event, expect_states, free_port, Supervisor,
};

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
        Self::spawn_with_args(&[])
    }

    fn spawn_with_args(extra: &[&str]) -> Self {
        let port = free_port();
        // Own process group: `uv run` wraps the actual python fake, and
        // killing only the wrapper leaves the device alive — kill() must
        // take the whole group for a dropout to actually happen.
        let child = Command::new("uv")
            .args([
                "run",
                "tests/fake_esphome.py",
                "--port",
                &port.to_string(),
                "--name",
                "shed",
            ])
            .args(extra)
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
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

    /// Kills the fake device and everything under it (the `uv` wrapper's
    /// process group), reaping the wrapper.
    fn kill(&mut self) {
        unsafe { libc::kill(-(self.child.id() as i32), libc::SIGKILL) };
        let _ = self.child.wait();
    }
}

impl Drop for FakeEsphome {
    fn drop(&mut self) {
        self.kill();
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
    setup_with(&[]).await
}

async fn setup_with(device_args: &[&str]) -> (FakeEsphome, PathBuf, Supervisor, zenoh::Session) {
    let device = FakeEsphome::spawn_with_args(device_args);
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

/// (a) The fake device's initial states translate to the correct home/state
/// keys: switch -> "on" (bool), sensor -> its device_class aspect, motion
/// binary_sensor -> presence capability's "occupancy" aspect.
#[tokio::test(flavor = "multi_thread")]
async fn device_state_translates_to_bus_state() {
    let (_device, _devices_path, mut sup, observer) = setup().await;
    let state_sub = observer.declare_subscriber("home/state/**").await.expect("state subscriber");

    // The device connects as soon as the adapter is up, so its initial
    // states may already be on the bus: read as a late joiner.
    await_states(
        &observer,
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

/// (a2) Availability rides the connection: every bound entity of the
/// device gets available = true once its entities are enumerated
/// (verified through the core mirror, the late-joiner path), and
/// available = false when the device drops.
#[tokio::test(flavor = "multi_thread")]
async fn device_dropout_flips_available() {
    let (mut device, _devices_path, mut sup, observer) = setup().await;
    let state_sub = observer
        .declare_subscriber("home/state/**")
        .await
        .expect("state subscriber");

    await_mirror(&observer, "home/state/shed/relay/available", &json!(true)).await;
    await_mirror(&observer, "home/state/shed/shed_temp/available", &json!(true)).await;
    await_mirror(&observer, "home/state/shed/shed_motion/available", &json!(true)).await;

    device.kill();

    expect_states(
        &state_sub,
        &[
            ("home/state/shed/relay/available", json!(false)),
            ("home/state/shed/shed_temp/available", json!(false)),
            ("home/state/shed/shed_motion/available", json!(false)),
        ],
    )
    .await;

    // A command at the dead device drops with device-unavailable
    // (docs/design.md, Sensor dropout) instead of silently vanishing.
    let event_sub = observer.declare_subscriber(EVENT_KEY).await.expect("event subscriber");
    observer
        .put(
            RELAY_CMD_KEY,
            json!({"value": true, "priority": "manual", "actor": "test"}).to_string(),
        )
        .await
        .expect("cmd put");
    expect_drop_event(&event_sub, "device-unavailable").await;

    sup.shutdown();
}

/// (a3) An ESPHome entity whose object_id would mint the reserved
/// `available` aspect drops with a health event while its siblings still
/// translate — the z2m twin of this rule has its own test in z2m.rs.
#[tokio::test(flavor = "multi_thread")]
async fn reserved_aspect_field_drops_with_health_event() {
    let (_device, _devices_path, mut sup, observer) = setup_with(&["--reserved-sensor"]).await;
    let event_sub = observer.declare_subscriber(EVENT_KEY).await.expect("event subscriber");
    let state_sub = observer.declare_subscriber(RELAY_STATE_KEY).await.expect("state subscriber");
    await_states(&observer, &state_sub, &[(RELAY_STATE_KEY, json!(false))]).await;

    // The relay command makes the device rebroadcast both the switch and
    // the reserved-aspect sensor: the switch translates, the reserved one
    // drops with a trace.
    observer
        .put(
            RELAY_CMD_KEY,
            json!({"value": true, "priority": "manual", "actor": "test"}).to_string(),
        )
        .await
        .expect("cmd put");
    expect_drop_event(&event_sub, "reserved-aspect").await;
    expect_states(&state_sub, &[(RELAY_STATE_KEY, json!(true))]).await;

    sup.shutdown();
}

/// (b) A manual-band cmd envelope on the switch reaches the fake device as
/// a SwitchCommandRequest, which echoes the new state back — landing on
/// the bus as the same translated state key.
#[tokio::test(flavor = "multi_thread")]
async fn cmd_envelope_reaches_fake_device_and_echoes_back() {
    let (_device, _devices_path, mut sup, observer) = setup().await;
    let state_sub = observer.declare_subscriber(RELAY_STATE_KEY).await.expect("state subscriber");
    await_states(&observer, &state_sub, &[(RELAY_STATE_KEY, json!(false))]).await;

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
    await_states(&observer, &state_sub, &[(RELAY_STATE_KEY, json!(false))]).await;

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
    // Published on connect, which races the liveliness token setup waited
    // for: read it the way every late joiner does, from the core mirror.
    let doc = await_discovery(&observer, "esphome", |doc| {
        doc.as_array().map(|r| r.len() == 3).unwrap_or(false)
    })
    .await;
    let records = doc.as_array().expect("discovery is an array");

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
    // The aspect descriptor (docs/design.md, Aspect descriptors) from the
    // same EntityInfo: the unit picks the kind, the ESPHome name the label.
    assert_eq!(
        temp["aspects"]["fields"]["temperature"],
        json!({"label": "temperature", "kind": "temperature", "group": "readings"})
    );
    assert_eq!(
        relay["aspects"]["fields"]["on"],
        json!({"label": "on", "kind": "boolean", "group": "readings"})
    );

    let motion = records
        .iter()
        .find(|r| r["id"] == json!("shed/motion"))
        .expect("motion record");
    assert_eq!(motion["configured"], json!(true));
    assert_eq!(motion["entity"], json!("shed_motion"));
    assert_eq!(motion["suggested"]["capability"], json!("presence"));
    assert_eq!(motion["description"]["device_class"], json!("motion"));
    assert_eq!(motion["aspects"]["fields"]["occupancy"]["label"], json!("motion (occupancy)"));
    assert_eq!(motion["aspects"]["fields"]["occupancy"]["values"][0], json!({"value": true, "label": "occupied"}));

    sup.shutdown();
}

/// (e) The adapter honors the step-2 unit contract: liveliness token when
/// ready, clean SIGTERM shutdown within the grace, no orphans.
#[tokio::test(flavor = "multi_thread")]
async fn adapter_honors_unit_contract() {
    let (_device, _devices_path, mut sup, observer) = setup().await;
    assert_unit_contract(&mut sup, &observer, "esphome").await;
}
