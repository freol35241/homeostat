//! Zigbee2MQTT adapter integration tests: each scenario spawns a real
//! mosquitto broker on a free port plus the real supervisor on the z2m
//! fixture house, and asserts on both buses.

mod common;

use std::time::Duration;

use homeostat::bus::HealthStatus;
use serde_json::{json, Value};
use zenoh::sample::SampleKind;

use common::{
    await_health, expect_drop_event, expect_states, health_watch, process_alive, Mosquitto, Mqtt,
    Supervisor,
};

const FIXTURE: &str = "tests/fixture_house_z2m";
const PORT_ENV: &str = "HOMEOSTAT_TEST_MQTT_PORT";
const EVENT_KEY: &str = "home/health/zigbee/event";
const LOCK_CMD_KEY: &str = "home/cmd/hallway/front_door/locked";

/// Spawns broker + supervisor on the fixture and waits for the adapter's
/// liveliness token (generous timeout: first run resolves the uv env).
async fn setup() -> (Mosquitto, Supervisor, zenoh::Session) {
    let mosquitto = Mosquitto::spawn();
    let port = mosquitto.port.to_string();
    let sup = Supervisor::spawn_with_env(FIXTURE, &[(PORT_ENV, &port)]);
    let observer = sup.observer().await;
    let token_sub = observer
        .liveliness()
        .declare_subscriber("home/health/zigbee/alive")
        .history(true)
        .await
        .expect("liveliness subscriber");
    let token = tokio::time::timeout(Duration::from_secs(60), token_sub.recv_async())
        .await
        .expect("adapter liveliness token within 60s")
        .expect("liveliness stream open");
    assert_eq!(token.kind(), SampleKind::Put);
    (mosquitto, sup, observer)
}

/// (a) Scripted z2m state publishes translate to per-aspect state keys.
#[tokio::test(flavor = "multi_thread")]
async fn z2m_state_translates_to_bus_state() {
    let (mosquitto, mut sup, observer) = setup().await;
    let state_sub = observer
        .declare_subscriber("home/state/**")
        .await
        .expect("state subscriber");
    let mut mqtt = Mqtt::connect(mosquitto.port, "test-state").await;

    mqtt.publish("zigbee2mqtt/lamp_kitchen_1", r#"{"state":"ON","brightness":128}"#)
        .await;
    expect_states(
        &state_sub,
        &[
            ("home/state/kitchen/kitchen_lamp/on", json!(true)),
            ("home/state/kitchen/kitchen_lamp/brightness", json!(128)),
        ],
    )
    .await;

    // Locks are state-only: state still translates (normalized to `locked`).
    mqtt.publish("zigbee2mqtt/lock_front_1", r#"{"state":"LOCKED"}"#).await;
    expect_states(&state_sub, &[("home/state/hallway/front_door/locked", json!(true))]).await;

    sup.shutdown();
}

/// (b) Bus commands translate to zigbee2mqtt/{device}/set. The lock's
/// arbitrated command path (home/cmd -> arbiter -> home/arbiter -> z2m) is
/// covered separately below, in `manual_lock_command_reaches_mqtt_via_arbiter`.
#[tokio::test(flavor = "multi_thread")]
async fn bus_commands_translate_to_z2m_set() {
    let (mosquitto, mut sup, observer) = setup().await;
    let mut mqtt = Mqtt::connect(mosquitto.port, "test-cmd").await;
    mqtt.subscribe("zigbee2mqtt/+/set").await;

    observer
        .put(
            "home/cmd/kitchen/kitchen_lamp/on",
            json!({"value": true, "priority": "manual", "actor": "test"}).to_string(),
        )
        .await
        .expect("cmd put");
    let (topic, payload) = mqtt
        .next_message(Duration::from_secs(10))
        .await
        .expect("set publish for on command");
    assert_eq!(topic, "zigbee2mqtt/lamp_kitchen_1/set");
    let payload: Value = serde_json::from_slice(&payload).expect("set payload is JSON");
    assert_eq!(payload, json!({"state": "ON"}));

    observer
        .put(
            "home/cmd/kitchen/kitchen_lamp/brightness",
            json!({"value": 200, "priority": "manual", "actor": "test"}).to_string(),
        )
        .await
        .expect("cmd put");
    let (topic, payload) = mqtt
        .next_message(Duration::from_secs(10))
        .await
        .expect("set publish for brightness command");
    assert_eq!(topic, "zigbee2mqtt/lamp_kitchen_1/set");
    let payload: Value = serde_json::from_slice(&payload).expect("set payload is JSON");
    assert_eq!(payload, json!({"brightness": 200}));

    sup.shutdown();
}

/// (c) Unknown devices and malformed payloads are dropped without crashing,
/// each with a health event, and translation keeps working afterwards.
#[tokio::test(flavor = "multi_thread")]
async fn bad_input_drops_with_health_event() {
    let (mosquitto, mut sup, observer) = setup().await;
    let event_sub = observer
        .declare_subscriber(EVENT_KEY)
        .await
        .expect("event subscriber");
    let state_sub = observer
        .declare_subscriber("home/state/**")
        .await
        .expect("state subscriber");
    let mut mqtt = Mqtt::connect(mosquitto.port, "test-bad").await;

    mqtt.publish("zigbee2mqtt/ghost_device", r#"{"state":"ON"}"#).await;
    expect_drop_event(&event_sub, "unknown-device").await;

    mqtt.publish("zigbee2mqtt/lamp_kitchen_1", "certainly not json").await;
    expect_drop_event(&event_sub, "malformed-payload").await;

    // Still alive and translating.
    mqtt.publish("zigbee2mqtt/lamp_kitchen_1", r#"{"state":"OFF"}"#).await;
    expect_states(&state_sub, &[("home/state/kitchen/kitchen_lamp/on", json!(false))]).await;

    sup.shutdown();
}

/// (c1b) Structurally malformed bridge/devices entries — non-dict rows, a
/// non-string id, a string definition, a non-list features — skip like
/// id-less ones instead of raising out of paho's network thread (which
/// would leave the adapter deaf but "running"); well-formed rows still
/// publish and translation keeps working afterwards.
#[tokio::test(flavor = "multi_thread")]
async fn malformed_inventory_entries_do_not_kill_the_translator() {
    let (mosquitto, mut sup, observer) = setup().await;
    let discovery_sub = observer
        .declare_subscriber("home/discovery/zigbee")
        .await
        .expect("discovery subscriber");
    let state_sub = observer
        .declare_subscriber("home/state/**")
        .await
        .expect("state subscriber");
    let mut mqtt = Mqtt::connect(mosquitto.port, "test-poison").await;

    mqtt.publish(
        "zigbee2mqtt/bridge/devices",
        r#"["stray", 7, {"friendly_name": {"nested": true}},
            {"friendly_name": "weird_def_1", "definition": "not-a-table"},
            {"friendly_name": "lamp_kitchen_1", "type": "Router",
             "definition": {"vendor": "acme",
                            "exposes": [{"type": "light", "features": "none"}]}}]"#,
    )
    .await;

    let sample = tokio::time::timeout(Duration::from_secs(10), discovery_sub.recv_async())
        .await
        .expect("discovery document within 10s")
        .expect("discovery sample");
    let doc: Value =
        serde_json::from_slice(&sample.payload().to_bytes()).expect("discovery is JSON");
    let records = doc.as_array().expect("discovery is an array");
    let ids: Vec<&str> = records.iter().filter_map(|r| r["id"].as_str()).collect();
    assert!(ids.contains(&"lamp_kitchen_1"), "well-formed row survives: {doc}");
    assert!(ids.contains(&"weird_def_1"), "string definition tolerated: {doc}");
    assert_eq!(records.len(), 2, "garbage rows skipped: {doc}");

    // The network thread survived the poison payload: still translating.
    mqtt.publish("zigbee2mqtt/lamp_kitchen_1", r#"{"state":"ON"}"#).await;
    expect_states(&state_sub, &[("home/state/kitchen/kitchen_lamp/on", json!(true))]).await;

    sup.shutdown();
}

/// (c2) The bridge's availability feature maps to the reserved `available`
/// aspect — both the {"state": ...} payload and the legacy bare string —
/// and a native device field that would mint the reserved aspect drops
/// with a health event while its siblings still translate.
#[tokio::test(flavor = "multi_thread")]
async fn availability_maps_to_reserved_aspect() {
    let (mosquitto, mut sup, observer) = setup().await;
    let event_sub = observer
        .declare_subscriber(EVENT_KEY)
        .await
        .expect("event subscriber");
    let state_sub = observer
        .declare_subscriber("home/state/**")
        .await
        .expect("state subscriber");
    let mut mqtt = Mqtt::connect(mosquitto.port, "test-availability").await;

    mqtt.publish("zigbee2mqtt/lamp_kitchen_1/availability", r#"{"state":"offline"}"#)
        .await;
    expect_states(
        &state_sub,
        &[("home/state/kitchen/kitchen_lamp/available", json!(false))],
    )
    .await;

    mqtt.publish("zigbee2mqtt/lamp_kitchen_1/availability", "online").await;
    expect_states(
        &state_sub,
        &[("home/state/kitchen/kitchen_lamp/available", json!(true))],
    )
    .await;

    mqtt.publish("zigbee2mqtt/lamp_kitchen_1", r#"{"available":true,"brightness":42}"#)
        .await;
    expect_drop_event(&event_sub, "reserved-aspect").await;
    expect_states(
        &state_sub,
        &[("home/state/kitchen/kitchen_lamp/brightness", json!(42))],
    )
    .await;

    sup.shutdown();
}

/// (d) The adapter honors the step-2 unit contract: liveliness token when
/// ready, clean SIGTERM shutdown within the grace, no orphans.
#[tokio::test(flavor = "multi_thread")]
async fn adapter_honors_unit_contract() {
    let (_mosquitto, mut sup, observer) = setup().await;
    let mut watch = health_watch(&observer, "zigbee").await;
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

/// (e) Discovery: a bridge/devices inventory lands on the bus as one JSON
/// document at home/discovery/zigbee — binding ids, configured flags,
/// best-effort suggestions, raw definitions; the coordinator is omitted.
#[tokio::test(flavor = "multi_thread")]
async fn bridge_inventory_published_as_discovery() {
    let (mosquitto, mut sup, observer) = setup().await;
    let mut mqtt = Mqtt::connect(mosquitto.port, "inventory-test").await;
    let sub = observer
        .declare_subscriber("home/discovery/zigbee")
        .await
        .expect("discovery subscriber");

    mqtt.publish(
        "zigbee2mqtt/bridge/devices",
        &json!([
            {"type": "Coordinator", "friendly_name": "Coordinator", "ieee_address": "0x00"},
            {"type": "Router", "friendly_name": "lamp_kitchen_1", "ieee_address": "0x01",
             "definition": {"vendor": "IKEA", "model": "LED1836G9", "description": "bulb",
                "exposes": [{"type": "light",
                    "features": [{"property": "state"}, {"property": "brightness"}]}]}},
            {"type": "EndDevice", "friendly_name": "motion_new", "ieee_address": "0x02",
             "definition": {"vendor": "Aqara", "model": "RTCGQ11LM", "description": "motion",
                "exposes": [{"type": "binary", "property": "occupancy"}]}}
        ])
        .to_string(),
    )
    .await;

    let sample = tokio::time::timeout(Duration::from_secs(10), sub.recv_async())
        .await
        .expect("discovery document within 10s")
        .expect("discovery sample");
    let doc: Value =
        serde_json::from_slice(&sample.payload().to_bytes()).expect("discovery is JSON");
    let records = doc.as_array().expect("discovery is an array");
    assert_eq!(records.len(), 2, "coordinator omitted: {doc}");

    let lamp = records
        .iter()
        .find(|r| r["id"] == json!("lamp_kitchen_1"))
        .expect("lamp record");
    assert_eq!(lamp["configured"], json!(true));
    assert_eq!(lamp["entity"], json!("kitchen_lamp"));
    assert_eq!(lamp["suggested"]["capability"], json!("light"));
    assert_eq!(lamp["suggested"]["features"], json!(["brightness"]));

    let motion = records
        .iter()
        .find(|r| r["id"] == json!("motion_new"))
        .expect("motion record");
    assert_eq!(motion["configured"], json!(false));
    assert_eq!(motion["entity"], json!(null));
    assert_eq!(motion["suggested"]["capability"], json!("presence"));
    assert_eq!(motion["description"]["model"], json!("RTCGQ11LM"));

    // The mirror serves it to late joiners: the read path the MCP
    // surface's read_state uses.
    let replies = observer.get("home/discovery/zigbee").await.expect("get discovery");
    let reply = replies.recv_async().await.expect("mirrored discovery reply");
    let mirrored: Value = serde_json::from_slice(
        &reply.result().expect("mirrored sample").payload().to_bytes(),
    )
    .expect("mirrored discovery is JSON");
    assert_eq!(mirrored, doc, "mirror serves the same document");

    sup.shutdown();
}

/// (f) The cmd contract, THE CONTRACT: every home/cmd/** payload is an
/// envelope `{value, priority, actor}`. A bare value with no envelope is
/// dropped with a health event (reason "invalid-command") instead of
/// reaching MQTT, and the adapter keeps translating afterwards.
#[tokio::test(flavor = "multi_thread")]
async fn envelope_less_command_drops_with_health_event() {
    let (mosquitto, mut sup, observer) = setup().await;
    let event_sub = observer
        .declare_subscriber(EVENT_KEY)
        .await
        .expect("event subscriber");
    let mut mqtt = Mqtt::connect(mosquitto.port, "test-no-envelope").await;
    mqtt.subscribe("zigbee2mqtt/+/set").await;

    // A bare value (the pre-envelope shape) on a cmd key is not an envelope.
    observer
        .put("home/cmd/kitchen/kitchen_lamp/on", "true")
        .await
        .expect("cmd put");
    expect_drop_event(&event_sub, "invalid-command").await;
    let silence = mqtt.next_message(Duration::from_millis(1500)).await;
    assert!(silence.is_none(), "envelope-less command reached MQTT: {silence:?}");

    // A properly enveloped command still works.
    observer
        .put(
            "home/cmd/kitchen/kitchen_lamp/on",
            json!({"value": false, "priority": "manual", "actor": "test"}).to_string(),
        )
        .await
        .expect("cmd put");
    let (topic, payload) = mqtt
        .next_message(Duration::from_secs(10))
        .await
        .expect("set publish for enveloped command");
    assert_eq!(topic, "zigbee2mqtt/lamp_kitchen_1/set");
    let payload: Value = serde_json::from_slice(&payload).expect("set payload is JSON");
    assert_eq!(payload, json!({"state": "OFF"}));

    sup.shutdown();
}

/// (g) Arbitrated lock command, end to end (docs/design.md, Arbitrated
/// mode): the fixture's front_door lock is arbitrated and the house runs
/// an arbiter unit. A manual-band wish on home/cmd forwards through the
/// arbiter to home/arbiter, which z2m now subscribes to and translates
/// into z2m's LOCK/UNLOCK set vocabulary. While that manual lease holds, a
/// direct automation-band wish on the same home/cmd key is refused
/// upstream and never reaches MQTT — which is also the structural proof
/// that z2m itself has no home/cmd subscription for the lock: if it did,
/// the refused wish would still leak through to MQTT regardless of the
/// arbiter's decision.
#[tokio::test(flavor = "multi_thread")]
async fn manual_lock_command_reaches_mqtt_via_arbiter() {
    let (mosquitto, mut sup, observer) = setup().await;
    let mut arbiter_watch = health_watch(&observer, "arbiter").await;
    await_health(&mut arbiter_watch, Duration::from_secs(60), |h| {
        h.status == HealthStatus::Running
    })
    .await;

    let mut mqtt = Mqtt::connect(mosquitto.port, "test-lock-cmd").await;
    mqtt.subscribe("zigbee2mqtt/+/set").await;

    let manual_wish = json!({"value": true, "priority": "manual", "actor": "test"});
    observer
        .put(LOCK_CMD_KEY, manual_wish.to_string())
        .await
        .expect("cmd put");
    let (topic, payload) = mqtt
        .next_message(Duration::from_secs(10))
        .await
        .expect("set publish for lock command");
    assert_eq!(topic, "zigbee2mqtt/lock_front_1/set");
    let payload: Value = serde_json::from_slice(&payload).expect("set payload is JSON");
    assert_eq!(payload, json!({"state": "LOCK"}));

    // The manual lease is now held (hold_minutes = 30 in the fixture): a
    // direct automation-band wish on the same key is refused upstream.
    let auto_wish = json!({"value": false, "priority": "automation", "actor": "scheduler"});
    observer
        .put(LOCK_CMD_KEY, auto_wish.to_string())
        .await
        .expect("cmd put");
    let silence = mqtt.next_message(Duration::from_millis(1500)).await;
    assert!(silence.is_none(), "refused automation wish reached MQTT: {silence:?}");

    sup.shutdown();
}
