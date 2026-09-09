//! Zigbee2MQTT adapter integration tests: each scenario spawns a real
//! mosquitto broker on a free port plus the real supervisor on the z2m
//! fixture house, and asserts on both buses.

mod common;

use std::time::Duration;

use homeostat::bus::HealthStatus;
use serde_json::{json, Value};
use zenoh::sample::SampleKind;

use common::{
    assert_unit_contract, await_health, expect_drop_event, expect_event_kind, expect_states, health_watch, Mosquitto, Mqtt, next_event, Supervisor, temp_house,
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
    assert_unit_contract(&mut sup, &observer, "zigbee").await;
}

/// (e) Discovery: a bridge/devices inventory lands on the bus as one JSON
/// document at home/discovery/zigbee — binding ids, configured flags,
/// best-effort suggestions, raw definitions; the coordinator is omitted.
/// A bound device's record also carries the aspect descriptor generated
/// from its exposes (docs/design.md, Aspect descriptors); an unbound one
/// does not.
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
                "exposes": [
                    {"type": "light", "features": [
                        {"type": "binary", "property": "state", "access": 7, "label": "State"},
                        {"type": "numeric", "property": "brightness", "value_min": 0, "value_max": 254, "access": 7}]},
                    {"type": "numeric", "property": "battery", "unit": "%", "category": "diagnostic", "access": 1},
                    {"type": "numeric", "property": "linkquality", "unit": "lqi", "category": "diagnostic", "access": 1, "label": "Linkquality"},
                    {"type": "enum", "property": "power_on_behavior", "values": ["off", "on", "previous"],
                     "access": 7, "category": "config", "label": "Power-on behavior"},
                    {"type": "binary", "property": "water_leak", "access": 1},
                    {"type": "composite", "property": "color", "features": []}]}},
            {"type": "EndDevice", "friendly_name": "motion_new", "ieee_address": "0x02",
             "definition": {"vendor": "Aqara", "model": "RTCGQ11LM", "description": "motion",
                "exposes": [{"type": "binary", "property": "occupancy"}]}},
            // z2m before 1.34: no `category` on any expose, battery first (#53)
            {"type": "EndDevice", "friendly_name": "shed_thermometer", "ieee_address": "0x03",
             "definition": {"vendor": "SONOFF", "model": "SNZB-02", "description": "thermometer",
                "exposes": [
                    {"type": "numeric", "property": "battery", "unit": "%", "access": 1},
                    {"type": "numeric", "property": "temperature", "unit": "°C", "access": 1},
                    {"type": "numeric", "property": "humidity", "unit": "%", "access": 1},
                    {"type": "numeric", "property": "voltage", "unit": "mV", "access": 1},
                    {"type": "numeric", "property": "linkquality", "unit": "lqi", "access": 1}]}}
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
    assert_eq!(records.len(), 3, "coordinator omitted: {doc}");

    let lamp = records
        .iter()
        .find(|r| r["id"] == json!("lamp_kitchen_1"))
        .expect("lamp record");
    assert_eq!(lamp["configured"], json!(true));
    assert_eq!(lamp["entity"], json!("kitchen_lamp"));
    assert_eq!(lamp["suggested"]["capability"], json!("light"));
    assert_eq!(lamp["suggested"]["features"], json!(["brightness"]));
    let fields = &lamp["aspects"]["fields"];
    assert_eq!(lamp["aspects"]["groups"], json!(["readings", "config", "diagnostics"]));
    // the capability's own vocabulary is described as readings, never commanded here
    assert_eq!(fields["on"], json!({"label": "state", "kind": "boolean", "group": "readings"}));
    assert_eq!(fields["brightness"], json!({"label": "brightness", "kind": "number", "group": "readings"}));
    // z2m's unit picks the kind; a plain number keeps its unit; battery is a reading
    assert_eq!(fields["battery"], json!({"label": "battery", "kind": "percent", "group": "readings"}));
    assert_eq!(fields["linkquality"], json!({"label": "linkquality", "kind": "number", "unit": "lqi", "group": "diagnostics"}));
    // a settable config expose is an owner-tier command with z2m's values
    assert_eq!(fields["power_on_behavior"]["group"], json!("config"));
    assert_eq!(fields["power_on_behavior"]["label"], json!("power-on behavior (power_on_behavior)"));
    assert_eq!(fields["power_on_behavior"]["command"], json!({"type": "enum", "editable_by": "owner"}));
    assert_eq!(fields["power_on_behavior"]["values"][2], json!({"value": "previous", "label": "previous"}));
    assert_eq!(fields["water_leak"]["notable"], json!(true));
    assert!(fields.get("color").is_none(), "composites are deferred, in state and descriptor alike");

    let motion = records
        .iter()
        .find(|r| r["id"] == json!("motion_new"))
        .expect("motion record");
    assert_eq!(motion["configured"], json!(false));
    assert_eq!(motion["entity"], json!(null));
    assert_eq!(motion["suggested"]["capability"], json!("presence"));
    assert_eq!(motion["description"]["model"], json!("RTCGQ11LM"));
    assert!(motion.get("aspects").is_none(), "an unbound device has no entity to describe");

    // Without categories the generator still sorts what newer z2m would
    // (linkquality, a battery voltage in mV → diagnostics), and battery,
    // a reading, goes last: the card headlines temperature and humidity.
    let thermo = records
        .iter()
        .find(|r| r["id"] == json!("shed_thermometer"))
        .expect("thermometer record");
    let fields = &thermo["aspects"]["fields"];
    assert_eq!(fields["temperature"]["group"], json!("readings"));
    assert_eq!(fields["humidity"]["group"], json!("readings"));
    assert_eq!(fields["battery"]["group"], json!("readings"));
    assert_eq!(fields["voltage"]["group"], json!("diagnostics"));
    assert_eq!(fields["linkquality"]["group"], json!("diagnostics"));
    let raw = String::from_utf8(sample.payload().to_bytes().to_vec()).expect("utf-8 payload");
    let thermo_raw = &raw[raw.find("\"id\": \"shed_thermometer\"").expect("thermometer in payload")..];
    let pos = |aspect: &str| thermo_raw.find(&format!("\"{aspect}\": {{")).expect(aspect);
    assert!(pos("temperature") < pos("humidity") && pos("humidity") < pos("battery"),
        "battery is ordered last among the readings: {thermo_raw}");

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

/// Copies the fixture with the adapter's base topic moved onto the
/// endpoint path (and optionally a short inventory timeout), so one
/// fixture serves the default and non-default prefixes alike. The
/// fixture's relative command has to become absolute: the copy lives in
/// a temp dir, not two levels under the repo.
fn house_with_base(tag: &str, base: &str, inventory_timeout_s: Option<f64>) -> std::path::PathBuf {
    let house = temp_house(FIXTURE, tag);
    let path = house.join("units/zigbee.toml");
    let repo = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut manifest = std::fs::read_to_string(&path).expect("read manifest");
    manifest = manifest.replace(
        "${HOMEOSTAT_TEST_MQTT_PORT}\"",
        &format!("${{HOMEOSTAT_TEST_MQTT_PORT}}/{base}\""),
    );
    manifest = manifest.replace(
        "uv run ../../adapters/",
        &format!("uv run {}/adapters/", repo.display()),
    );
    if let Some(timeout) = inventory_timeout_s {
        manifest.push_str(&format!(
            "\n[params.inventory_timeout_s]\ntype = \"float\"\ndefault = {timeout}\n\
             editable_by = \"owner\"\n"
        ));
    }
    std::fs::write(&path, manifest).expect("write manifest");
    house
}

/// Spawns broker + supervisor on a base-topic variant of the fixture.
async fn setup_at(house: &std::path::Path) -> (Mosquitto, Supervisor, zenoh::Session) {
    let mosquitto = Mosquitto::spawn();
    let sup = Supervisor::spawn_at(house, &[(PORT_ENV, &mosquitto.port.to_string())]);
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

/// (h) A non-default, multi-segment base topic works in every direction.
/// The prefix comes from the endpoint path, and every topic the adapter
/// parses is relative to it — splitting at a fixed segment would break the
/// moment the prefix carries its own slash.
#[tokio::test(flavor = "multi_thread")]
async fn a_non_default_base_topic_translates_both_directions() {
    let house = house_with_base("z2m-prefixed", "VP52/zigbee2mqtt", None);
    let (mosquitto, mut sup, observer) = setup_at(&house).await;
    let state_sub = observer
        .declare_subscriber("home/state/**")
        .await
        .expect("state subscriber");
    let discovery_sub = observer
        .declare_subscriber("home/discovery/zigbee")
        .await
        .expect("discovery subscriber");
    let mut mqtt = Mqtt::connect(mosquitto.port, "test-prefixed").await;

    mqtt.publish(
        "VP52/zigbee2mqtt/lamp_kitchen_1",
        r#"{"state":"ON","brightness":128}"#,
    )
    .await;
    expect_states(
        &state_sub,
        &[
            ("home/state/kitchen/kitchen_lamp/on", json!(true)),
            ("home/state/kitchen/kitchen_lamp/brightness", json!(128)),
        ],
    )
    .await;

    // Availability: the topic now has one more slash than the default
    // prefix produces, which the old fixed-position parse mis-read.
    mqtt.publish(
        "VP52/zigbee2mqtt/lamp_kitchen_1/availability",
        r#"{"state":"online"}"#,
    )
    .await;
    expect_states(
        &state_sub,
        &[("home/state/kitchen/kitchen_lamp/available", json!(true))],
    )
    .await;

    mqtt.publish(
        "VP52/zigbee2mqtt/bridge/devices",
        r#"[{"ieee_address":"0x1","friendly_name":"lamp_kitchen_1","definition":null}]"#,
    )
    .await;
    let sample = tokio::time::timeout(Duration::from_secs(10), discovery_sub.recv_async())
        .await
        .expect("discovery document within 10s")
        .expect("discovery sample");
    let discovery: Value =
        serde_json::from_slice(&sample.payload().to_bytes()).expect("discovery is JSON");
    assert!(
        discovery.to_string().contains("lamp_kitchen_1"),
        "inventory republished under the moved prefix: {discovery}"
    );

    // And commands go out under the same prefix.
    let mut set_sub = Mqtt::connect(mosquitto.port, "test-prefixed-set").await;
    set_sub.subscribe("VP52/zigbee2mqtt/+/set").await;
    observer
        .put(
            "home/cmd/kitchen/kitchen_lamp/on",
            json!({"value": true, "priority": "manual", "actor": "test"}).to_string(),
        )
        .await
        .expect("cmd put");
    let (topic, payload) = set_sub
        .next_message(Duration::from_secs(10))
        .await
        .expect("set publish for on command");
    assert_eq!(topic, "VP52/zigbee2mqtt/lamp_kitchen_1/set");
    assert_eq!(
        serde_json::from_slice::<Value>(&payload).expect("set payload is JSON"),
        json!({"state": "ON"})
    );

    sup.shutdown();
    let _ = std::fs::remove_dir_all(&house);
}

/// (i) A base topic that matches nothing subscribes SUCCESSFULLY and then
/// hears nothing — no SUBACK timeout, no error. The adapter must say so
/// rather than sit there healthy and permanently deaf.
#[tokio::test(flavor = "multi_thread")]
async fn a_base_topic_that_matches_nothing_reports_bridge_silent() {
    let house = house_with_base("z2m-deaf", "wrong/prefix", Some(1.0));
    let (mosquitto, mut sup, observer) = setup_at(&house).await;
    let event_sub = observer
        .declare_subscriber(EVENT_KEY)
        .await
        .expect("event subscriber");

    // The estate is alive under the real prefix; the adapter is listening
    // somewhere else entirely.
    let mut mqtt = Mqtt::connect(mosquitto.port, "test-deaf").await;
    mqtt.publish(
        "zigbee2mqtt/bridge/devices",
        r#"[{"ieee_address":"0x1","friendly_name":"lamp_kitchen_1","definition":null}]"#,
    )
    .await;

    expect_event_kind(&event_sub, "bridge-silent").await;
    let mut watch = health_watch(&observer, "zigbee").await;
    await_health(&mut watch, Duration::from_secs(10), |h| {
        h.status == HealthStatus::Running
    })
    .await;

    sup.shutdown();
    let _ = std::fs::remove_dir_all(&house);
}

/// (j) A broker that requires auth: the password reaches the adapter from
/// HOMEOSTAT_MQTT_CREDENTIALS, a file outside the repo. The manifest keeps
/// no secret, and the password is one URL parsing would mangle — `@` and
/// `/` in an inline mqtt://user:pass@host silently reparse the host.
#[tokio::test(flavor = "multi_thread")]
async fn broker_credentials_come_from_a_file_outside_the_repo() {
    const USER: &str = "homeostat";
    const PASSWORD: &str = "p@ss/w0rd#1";

    let mosquitto = Mosquitto::spawn_with_auth(USER, PASSWORD);
    let creds = std::env::temp_dir().join(format!("homeostat-mqtt-creds-{}.toml", std::process::id()));
    std::fs::write(
        &creds,
        format!("[\"127.0.0.1\"]\nusername = \"{USER}\"\npassword = \"{PASSWORD}\"\n"),
    )
    .expect("write credentials");

    let sup = Supervisor::spawn_with_env(
        FIXTURE,
        &[
            (PORT_ENV, &mosquitto.port.to_string()),
            ("HOMEOSTAT_MQTT_CREDENTIALS", creds.to_str().expect("utf-8")),
        ],
    );
    let observer = sup.observer().await;
    let token_sub = observer
        .liveliness()
        .declare_subscriber("home/health/zigbee/alive")
        .history(true)
        .await
        .expect("liveliness subscriber");
    let token = tokio::time::timeout(Duration::from_secs(60), token_sub.recv_async())
        .await
        .expect("adapter connects to the authenticated broker within 60s")
        .expect("liveliness stream open");
    assert_eq!(token.kind(), SampleKind::Put);

    // Proof it is really talking to the broker, not merely alive.
    let state_sub = observer
        .declare_subscriber("home/state/**")
        .await
        .expect("state subscriber");
    let mut mqtt = Mqtt::connect_auth(mosquitto.port, "test-auth", USER, PASSWORD).await;
    mqtt.publish("zigbee2mqtt/lamp_kitchen_1", r#"{"state":"ON"}"#).await;
    expect_states(&state_sub, &[("home/state/kitchen/kitchen_lamp/on", json!(true))]).await;

    let manifest = std::fs::read_to_string(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(FIXTURE).join("units/zigbee.toml"),
    )
    .expect("read manifest");
    assert!(
        !manifest.contains(PASSWORD),
        "the password must never sit in a unit manifest"
    );

    let mut sup = sup;
    sup.shutdown();
    let _ = std::fs::remove_file(&creds);
}

/// (k) A device the bridge knows but no entity file binds is a steady
/// state, not a dropped message: it must not emit an event per publish.
/// The discovery-first workflow makes that the NORMAL condition, so the
/// event would run forever on a house mid-configuration.
#[tokio::test(flavor = "multi_thread")]
async fn a_known_but_unbound_device_does_not_report_every_message() {
    let (mosquitto, mut sup, observer) = setup().await;
    let event_sub = observer
        .declare_subscriber(EVENT_KEY)
        .await
        .expect("event subscriber");
    let discovery_sub = observer
        .declare_subscriber("home/discovery/zigbee")
        .await
        .expect("discovery subscriber");
    let mut mqtt = Mqtt::connect(mosquitto.port, "test-unbound").await;

    // The bridge knows snzb_03_01; the fixture binds no entity to it.
    mqtt.publish(
        "zigbee2mqtt/bridge/devices",
        r#"[{"ieee_address":"0x1","friendly_name":"snzb_03_01","definition":null}]"#,
    )
    .await;
    tokio::time::timeout(Duration::from_secs(10), discovery_sub.recv_async())
        .await
        .expect("inventory processed within 10s")
        .expect("discovery sample");

    for _ in 0..3 {
        mqtt.publish("zigbee2mqtt/snzb_03_01", r#"{"occupancy":true}"#).await;
    }
    // A sentinel the adapter definitely reports, published last. The
    // assertion is strict on the NEXT event: any unknown-device emitted
    // for snzb_03_01 would arrive ahead of it.
    mqtt.publish("zigbee2mqtt/lamp_kitchen_1", "certainly not json").await;
    assert_eq!(
        next_event(&event_sub).await["reason"],
        json!("malformed-payload"),
        "a known-but-unbound device must not report each publish"
    );

    // ...whereas a device the bridge has never mentioned still reports.
    mqtt.publish("zigbee2mqtt/ghost_device", r#"{"state":"ON"}"#).await;
    assert_eq!(next_event(&event_sub).await["reason"], json!("unknown-device"));

    sup.shutdown();
    drop(mosquitto);
}

/// (l) Mid-run bridge liveness. The boot watchdog is one-shot and cannot
/// see a bridge that dies later; the bridge's own retained state can, and
/// the inventory cannot — z2m republishes it only on change, so its
/// silence never distinguishes a dead bridge from a stable estate.
#[tokio::test(flavor = "multi_thread")]
async fn a_bridge_going_offline_mid_run_reports_once_per_transition() {
    let (mosquitto, mut sup, observer) = setup().await;
    let event_sub = observer
        .declare_subscriber(EVENT_KEY)
        .await
        .expect("event subscriber");
    let mut mqtt = Mqtt::connect(mosquitto.port, "test-bridge-state").await;

    // A healthy bridge says so, repeatedly, and that is not an event.
    for _ in 0..2 {
        mqtt.publish("zigbee2mqtt/bridge/state", r#"{"state":"online"}"#).await;
    }
    mqtt.publish("zigbee2mqtt/bridge/state", r#"{"state":"offline"}"#).await;
    let event = next_event(&event_sub).await;
    assert_eq!(event["kind"], json!("bridge-silent"), "{event}");
    assert_eq!(event["state"], json!("offline"), "{event}");

    // Still offline is not a new transition; the legacy bare-string form
    // is understood the same way device availability understands it.
    mqtt.publish("zigbee2mqtt/bridge/state", "offline").await;
    mqtt.publish("zigbee2mqtt/bridge/state", "certainly not a state").await;
    let event = next_event(&event_sub).await;
    assert_eq!(
        event["reason"],
        json!("malformed-payload"),
        "a repeat offline must not re-report: {event}"
    );

    sup.shutdown();
    drop(mosquitto);
}
