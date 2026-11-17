//! ESPHome adapter integration tests: each scenario spawns a fake ESPHome
//! device (`tests/fake_esphome.py`, which speaks the real plaintext wire
//! protocol over aioesphomeapi's bundled protobuf messages) on a free port
//! plus the real supervisor on the esphome fixture house, and asserts on
//! both buses.

mod common;

use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use zenoh::sample::SampleKind;

use common::{
    assert_unit_contract, await_discovery, await_health, await_mirror, await_states,
    expect_drop_event, expect_states, free_port, health_watch, Supervisor,
};
use homeostat::bus::HealthStatus;

const FIXTURE: &str = "tests/fixtures/house_esphome";
/// The same device with its relay arbitrated and reporting its causes.
const CAUSES_FIXTURE: &str = "tests/fixtures/house_esphome_causes";
const DEVICES_ENV: &str = "HOMEOSTAT_ESPHOME_DEVICES";
const EVENT_KEY: &str = "home/health/esphome/event";
const RELAY_STATE_KEY: &str = "home/state/shed/relay/on";
const RELAY_CMD_KEY: &str = "home/cmd/shed/relay/on";

/// A fake ESPHome device (`tests/fake_esphome.py`) on a free port, killed on
/// drop. Spawned the same way the units themselves are: `uv run`.
struct FakeEsphome {
    child: Child,
    port: u16,
}

impl FakeEsphome {
    #[allow(dead_code)]
    fn spawn() -> Self {
        Self::spawn_with_args(&[])
    }

    fn spawn_with_args(extra: &[&str]) -> Self {
        let port = free_port();
        // Own process group: `uv run` wraps the python fake, and killing only
        // the wrapper leaves the device alive. kill() takes the whole group so
        // that the dropout happens.
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
        let deadline = Instant::now() + Duration::from_mins(1);
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(
                Instant::now() < deadline,
                "fake esphome device never listened on {port}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        Self { child, port }
    }

    /// Kills the fake device and everything under it (the `uv` wrapper's
    /// process group), reaping the wrapper.
    fn kill(&mut self) {
        unsafe {
            libc::kill(
                -i32::try_from(self.child.id()).expect("a pid fits i32"),
                libc::SIGKILL,
            )
        };
        let _ = self.child.wait();
    }
}

impl Drop for FakeEsphome {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Writes a `HOMEOSTAT_ESPHOME_DEVICES` file (outside the repo, where
/// device addresses and keys live) giving the fixture's "shed" device a
/// host override at the fake device's port.
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
    setup_at(FIXTURE, device_args).await
}

async fn setup_at(
    fixture: &str,
    device_args: &[&str],
) -> (FakeEsphome, PathBuf, Supervisor, zenoh::Session) {
    let house = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(fixture);
    setup_in(&house, device_args).await
}

/// As `setup_at`, on a house at any path.
async fn setup_in(
    house: &std::path::Path,
    device_args: &[&str],
) -> (FakeEsphome, PathBuf, Supervisor, zenoh::Session) {
    let device = FakeEsphome::spawn_with_args(device_args);
    let devices_path = devices_file(device.port);
    let sup = Supervisor::spawn_at(
        house,
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
/// keys: switch -> "on" (bool), sensor -> its `device_class` aspect, motion
/// `binary_sensor` -> the occupancy capability's "motion" aspect.
#[tokio::test(flavor = "multi_thread")]
async fn device_state_translates_to_bus_state() {
    let (_device, _devices_path, mut sup, observer) = setup().await;
    let state_sub = observer
        .declare_subscriber("home/state/**")
        .await
        .expect("state subscriber");

    // The device connects as soon as the adapter is up, so its initial
    // states may already be on the bus: read as a late joiner.
    await_states(
        &observer,
        &state_sub,
        &[
            (RELAY_STATE_KEY, json!(false)),
            ("home/state/shed/shed_temp/temperature", json!(21.5)),
            ("home/state/shed/shed_motion/motion", json!(true)),
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
    await_mirror(
        &observer,
        "home/state/shed/shed_temp/available",
        &json!(true),
    )
    .await;
    await_mirror(
        &observer,
        "home/state/shed/shed_motion/available",
        &json!(true),
    )
    .await;

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
    // (docs/design.md#availability) instead of disappearing without a trace.
    let event_sub = observer
        .declare_subscriber(EVENT_KEY)
        .await
        .expect("event subscriber");
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

/// (a3) An ESPHome entity whose `object_id` would mint the reserved
/// `available` aspect drops with a health event while its siblings still
/// translate. z2m.rs tests the same rule for Zigbee.
#[tokio::test(flavor = "multi_thread")]
async fn reserved_aspect_field_drops_with_health_event() {
    let (_device, _devices_path, mut sup, observer) = setup_with(&["--reserved-sensor"]).await;
    let event_sub = observer
        .declare_subscriber(EVENT_KEY)
        .await
        .expect("event subscriber");
    let state_sub = observer
        .declare_subscriber(RELAY_STATE_KEY)
        .await
        .expect("state subscriber");
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
/// a `SwitchCommandRequest`, which echoes the new state back. The echo
/// lands on the bus as the same translated state key.
#[tokio::test(flavor = "multi_thread")]
async fn cmd_envelope_reaches_fake_device_and_echoes_back() {
    let (_device, _devices_path, mut sup, observer) = setup().await;
    let state_sub = observer
        .declare_subscriber(RELAY_STATE_KEY)
        .await
        .expect("state subscriber");
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

/// An ESPHome `event` entity binds as a `button` (docs/design.md#buttons):
/// each event it fires is published as `action`, the same event twice
/// included, nothing is published for it on connect, and discovery lists
/// the event types it can fire.
#[tokio::test(flavor = "multi_thread")]
async fn an_event_entity_publishes_every_press() {
    const ACTION_KEY: &str = "home/state/shed/shed_button/action";
    let (_device, _devices_path, mut sup, observer) = setup_with(&["--button"]).await;
    let doc = await_discovery(&observer, "esphome", |doc| {
        doc.as_array().is_some_and(|r| r.len() == 4)
    })
    .await;
    let button = doc
        .as_array()
        .expect("discovery is an array")
        .iter()
        .find(|r| r["id"] == json!("shed/button"))
        .expect("button record")
        .clone();
    assert_eq!(button["suggested"]["capability"], json!("button"));
    assert_eq!(
        button["description"]["event_types"],
        json!(["single", "double", "long"])
    );
    assert_eq!(
        button["aspects"]["fields"]["action"]["values"][1],
        json!({"value": "double", "label": "double"})
    );

    let state_sub = observer
        .declare_subscriber(RELAY_STATE_KEY)
        .await
        .expect("relay subscriber");
    await_states(&observer, &state_sub, &[(RELAY_STATE_KEY, json!(false))]).await;
    assert_eq!(
        common::cache_read(&observer, ACTION_KEY).await,
        None,
        "a connect is not a press"
    );

    let action_sub = observer
        .declare_subscriber(ACTION_KEY)
        .await
        .expect("action subscriber");
    for value in [true, false] {
        observer
            .put(
                RELAY_CMD_KEY,
                json!({"value": value, "priority": "manual", "actor": "test"}).to_string(),
            )
            .await
            .expect("cmd put");
    }
    let mut presses = Vec::new();
    for _ in 0..2 {
        let sample = tokio::time::timeout(Duration::from_secs(20), action_sub.recv_async())
            .await
            .expect("a press within 20s")
            .expect("stream open");
        presses.push(
            serde_json::from_slice::<serde_json::Value>(&sample.payload().to_bytes())
                .expect("JSON"),
        );
    }
    assert_eq!(
        presses,
        [json!("single"), json!("single")],
        "a repeat is a press"
    );

    sup.shutdown();
}

/// (c) The cmd contract: a bare value with no envelope drops with an
/// "invalid-command" health event instead of reaching the device, and a
/// properly enveloped command afterwards still works.
#[tokio::test(flavor = "multi_thread")]
async fn envelope_less_command_drops_with_health_event() {
    let (_device, _devices_path, mut sup, observer) = setup().await;
    let event_sub = observer
        .declare_subscriber(EVENT_KEY)
        .await
        .expect("event subscriber");
    let state_sub = observer
        .declare_subscriber(RELAY_STATE_KEY)
        .await
        .expect("state subscriber");
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
        doc.as_array().is_some_and(|r| r.len() == 3)
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
    // The aspect descriptor (docs/design.md#aspect-descriptors) from the
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
    assert_eq!(motion["suggested"]["capability"], json!("occupancy"));
    assert_eq!(motion["description"]["device_class"], json!("motion"));
    assert_eq!(
        motion["aspects"]["fields"]["motion"]["label"],
        json!("motion")
    );
    assert_eq!(
        motion["aspects"]["fields"]["motion"]["values"][0],
        json!({"value": true, "label": "motion"})
    );

    sup.shutdown();
}

/// (e) The adapter honors the unit contract: liveliness token when
/// ready, clean SIGTERM shutdown within the grace, no orphans.
#[tokio::test(flavor = "multi_thread")]
async fn adapter_honors_unit_contract() {
    let (_device, _devices_path, mut sup, observer) = setup().await;
    assert_unit_contract(&mut sup, &observer, "esphome").await;
}

/// docs/design.md#device-reported-causes: a change after a person's cause
/// is reported as the family's, so the arbiter holds it, and one after a
/// rule's cause is recorded as the device's and clears the family's hold.
#[tokio::test(flavor = "multi_thread")]
async fn a_cause_the_device_reports_decides_who_changed_the_relay() {
    const HOLD_KEY: &str = "home/hold/arbiter/shed/relay/on";
    let trigger = std::env::temp_dir().join(format!(
        "homeostat-esphome-causes-{}.txt",
        std::process::id()
    ));
    std::fs::write(&trigger, "").expect("trigger file");
    let (_device, _devices_path, mut sup, observer) = setup_at(
        CAUSES_FIXTURE,
        &["--causes", trigger.to_str().expect("utf-8 path")],
    )
    .await;
    let mut watch = health_watch(&observer, "arbiter").await;
    await_health(&mut watch, Duration::from_mins(1), |h| {
        h.status == HealthStatus::Running
    })
    .await;
    let hold_sub = observer
        .declare_subscriber(HOLD_KEY)
        .await
        .expect("hold subscriber");
    let event_sub = observer
        .declare_subscriber(EVENT_KEY)
        .await
        .expect("event subscriber");
    let state_sub = observer
        .declare_subscriber(RELAY_STATE_KEY)
        .await
        .expect("relay subscriber");
    await_states(&observer, &state_sub, &[(RELAY_STATE_KEY, json!(false))]).await;

    // The button on the board: the family holds the relay on.
    std::fs::write(&trigger, "button").expect("press");
    let sample = tokio::time::timeout(Duration::from_secs(20), hold_sub.recv_async())
        .await
        .expect("a family hold within 20s")
        .expect("stream open");
    assert_eq!(sample.kind(), SampleKind::Put);
    let record: serde_json::Value =
        serde_json::from_slice(&sample.payload().to_bytes()).expect("JSON");
    assert_eq!(
        (&record["value"], &record["priority"], &record["actor"]),
        (&json!(true), &json!("manual"), &json!("esphome"))
    );

    // The board's own timer: recorded as its rule, and the family's hold,
    // whose value the device left, is cleared.
    std::fs::write(&trigger, "valve_timeout").expect("timer");
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut rule = None;
    while rule.is_none() && Instant::now() < deadline {
        let Ok(Ok(sample)) =
            tokio::time::timeout(Duration::from_secs(1), event_sub.recv_async()).await
        else {
            continue;
        };
        let event: serde_json::Value =
            serde_json::from_slice(&sample.payload().to_bytes()).expect("JSON");
        if event["kind"] == json!("device-rule") {
            rule = Some(event);
        }
    }
    let rule = rule.expect("a device-rule event");
    assert_eq!(
        (&rule["entity"], &rule["rule"], &rule["value"]),
        (&json!("relay"), &json!("valve_timeout"), &json!(false))
    );
    let sample = tokio::time::timeout(Duration::from_secs(20), hold_sub.recv_async())
        .await
        .expect("the family hold ends within 20s")
        .expect("stream open");
    assert_eq!(
        sample.kind(),
        SampleKind::Delete,
        "the family's hold is cleared"
    );

    sup.shutdown();
    let _ = std::fs::remove_file(&trigger);
}

/// docs/design.md#device-timed-runtime: a switch with the `runtime_s`
/// feature reads and sets the device's `{object_id}_runtime_s` number,
/// within the number's own bounds.
#[tokio::test(flavor = "multi_thread")]
async fn a_switch_runtime_is_the_devices_number() {
    const RUNTIME_KEY: &str = "home/state/shed/relay/runtime_s";
    let house = common::temp_house(FIXTURE, "esphome-runtime");
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("adapters/esphome.py");
    let manifest = house.join("units/esphome.toml");
    let text = std::fs::read_to_string(&manifest).expect("manifest");
    std::fs::write(
        &manifest,
        text.replace(
            "uv run ../../../adapters/esphome.py",
            &format!("uv run {}", script.display()),
        ),
    )
    .expect("manifest");
    let relay = house.join("entities/esphome/relay.toml");
    let text = std::fs::read_to_string(&relay).expect("relay");
    std::fs::write(
        &relay,
        text.replace(
            "capability = \"switch\"",
            "capability = \"switch\"\nfeatures = [\"runtime_s\"]",
        ),
    )
    .expect("relay");
    let (_device, _devices_path, mut sup, observer) = setup_in(&house, &["--runtime"]).await;

    let state_sub = observer
        .declare_subscriber(RUNTIME_KEY)
        .await
        .expect("runtime subscriber");
    await_states(&observer, &state_sub, &[(RUNTIME_KEY, json!(600.0))]).await;
    let doc = await_discovery(&observer, "esphome", |doc| {
        doc.as_array().is_some_and(|records| {
            records.iter().any(|r| {
                r["id"] == json!("shed/relay") && r["aspects"]["fields"]["runtime_s"].is_object()
            })
        })
    })
    .await;
    let relay = doc
        .as_array()
        .expect("records")
        .iter()
        .find(|r| r["id"] == json!("shed/relay"))
        .expect("relay record")
        .clone();
    assert_eq!(
        relay["aspects"]["fields"]["runtime_s"]["command"]["constraint"],
        json!({"min": 1.0, "max": 3600.0})
    );

    let event_sub = observer
        .declare_subscriber(EVENT_KEY)
        .await
        .expect("event subscriber");
    let cmd =
        |value: Value| json!({"value": value, "priority": "manual", "actor": "test"}).to_string();
    observer
        .put("home/cmd/shed/relay/runtime_s", cmd(json!(120)))
        .await
        .expect("cmd put");
    await_states(&observer, &state_sub, &[(RUNTIME_KEY, json!(120.0))]).await;
    observer
        .put("home/cmd/shed/relay/runtime_s", cmd(json!(99_999)))
        .await
        .expect("cmd put");
    expect_drop_event(&event_sub, "invalid-command").await;

    sup.shutdown();
}
