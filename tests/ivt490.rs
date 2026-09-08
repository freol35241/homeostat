//! IVT490 heat-pump adapter integration tests: each scenario spawns a real
//! mosquitto broker on a free port plus the real supervisor on the ivt490
//! fixture house, and asserts on both buses.

mod common;

use std::time::Duration;

use homeostat::bus::HealthStatus;
use serde_json::json;
use zenoh::sample::SampleKind;

use common::{
    await_health, expect_drop_event, expect_event_kind, expect_states, health_watch,
    process_alive, Mosquitto, Mqtt, Supervisor,
};

const FIXTURE: &str = "tests/fixture_house_ivt490";
const PORT_ENV: &str = "HOMEOSTAT_TEST_MQTT_PORT";
const BASE: &str = "ivt490_1"; // the fixture entity's base topic (its `id`)
const EVENT_KEY: &str = "home/health/ivt490/event";
const SETPOINT_CMD_KEY: &str = "home/cmd/utility/heatpump/setpoint";
const SETPOINT_ARBITER_KEY: &str = "home/arbiter/utility/heatpump/setpoint";
const SETPOINT_SET_TOPIC: &str = "ivt490_1/controller/set/indoor_temperature_target";
const AVAILABLE_KEY: &str = "home/state/utility/heatpump/available";
const FEED_SOURCE_KEY: &str = "home/state/global/indoor_temperature/temperature";
const FEED_SOURCE_AVAILABLE_KEY: &str = "home/state/global/indoor_temperature/available";
const FEED_SET_TOPIC: &str = "ivt490_1/controller/set/indoor_temperature_actual";

/// Spawns broker + supervisor on the fixture and waits for the adapter's
/// liveliness token (generous timeout: first run resolves the uv env).
async fn setup() -> (Mosquitto, Supervisor, zenoh::Session) {
    let mosquitto = Mosquitto::spawn();
    let port = mosquitto.port.to_string();
    let sup = Supervisor::spawn_with_env(FIXTURE, &[(PORT_ENV, &port)]);
    let observer = sup.observer().await;
    let token_sub = observer
        .liveliness()
        .declare_subscriber("home/health/ivt490/alive")
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

/// (a) Scripted per-field state publishes translate to normalized and
/// passthrough bus aspects: the "serial" wrapper level is stripped, the
/// sensor-object leaves join with underscores, a controller field's
/// `valid` predicate becomes its own {aspect}_valid boolean, and nested
/// blob topics and the raw serial line produce nothing — not even a
/// health event.
#[tokio::test(flavor = "multi_thread")]
async fn ivt490_state_translates_to_bus_state() {
    let (mosquitto, mut sup, observer) = setup().await;
    let state_sub = observer
        .declare_subscriber("home/state/**")
        .await
        .expect("state subscriber");
    let event_sub = observer
        .declare_subscriber(EVENT_KEY)
        .await
        .expect("event subscriber");
    let mut mqtt = Mqtt::connect(mosquitto.port, "test-state").await;

    // Serial GT1 (Framledningstemperatur) normalizes to feed_temperature.
    mqtt.publish(&format!("{BASE}/ivt490/state/serial/GT1"), "21.50").await;
    // A plain serial field passes through under its firmware name.
    mqtt.publish(&format!("{BASE}/ivt490/state/serial/GT5"), "19.80").await;
    // A thermistor sensor leaf joins its path with underscores.
    mqtt.publish(&format!("{BASE}/ivt490/state/GT2/filtered"), "5.30").await;
    // The serial sub-blob (an object) is skipped: its leaves arrive on the
    // deeper subtopics above.
    mqtt.publish(
        &format!("{BASE}/ivt490/state/serial"),
        r#"{"GT1":21.5,"GT5":19.8}"#,
    )
    .await;
    // The controller's indoor_temperature_feedback normalizes to
    // indoor_temperature; its {value, valid} nesting is unwrapped, the
    // flag surfacing as indoor_temperature_valid.
    mqtt.publish(
        &format!("{BASE}/controller/state/indoor_temperature_feedback"),
        r#"{"value":20.30,"valid":true}"#,
    )
    .await;
    // A field the device has stopped believing still publishes its value —
    // stale and absent must stay distinguishable — with the flag false.
    mqtt.publish(
        &format!("{BASE}/controller/state/outdoor_temperature_offset"),
        r#"{"value":-2.50,"valid":false}"#,
    )
    .await;
    // The controller's indoor_temperature_target normalizes to setpoint —
    // the device's own readback ({value}-nested on the wire), not a
    // command echo.
    mqtt.publish(
        &format!("{BASE}/controller/state/indoor_temperature_target"),
        r#"{"value":21.00}"#,
    )
    .await;
    // operating_mode is a bare scalar and passes through under its name.
    mqtt.publish(&format!("{BASE}/controller/state/operating_mode"), "1").await;
    // The unparsed serial line is on an unsubscribed topic: nothing at all.
    mqtt.publish(&format!("{BASE}/ivt490/raw"), "0;219;not-json-at-all").await;

    expect_states(
        &state_sub,
        &[
            ("home/state/utility/heatpump/feed_temperature", json!(21.5)),
            ("home/state/utility/heatpump/GT5", json!(19.8)),
            ("home/state/utility/heatpump/GT2_filtered", json!(5.3)),
            ("home/state/utility/heatpump/indoor_temperature", json!(20.3)),
            ("home/state/utility/heatpump/indoor_temperature_valid", json!(true)),
            ("home/state/utility/heatpump/outdoor_temperature_offset", json!(-2.5)),
            (
                "home/state/utility/heatpump/outdoor_temperature_offset_valid",
                json!(false),
            ),
            ("home/state/utility/heatpump/setpoint", json!(21.0)),
            ("home/state/utility/heatpump/operating_mode", json!(1)),
        ],
    )
    .await;

    let no_event = tokio::time::timeout(Duration::from_millis(1500), event_sub.recv_async()).await;
    assert!(no_event.is_err(), "unexpected health event for blob/raw topics");

    sup.shutdown();
}

/// (a2) The receive-timer availability: a routed message flips
/// available = true, availability_timeout_s (5 s in the fixture) of
/// silence flips it false with a "device-silent" health event, and the
/// next message flips it back.
#[tokio::test(flavor = "multi_thread")]
async fn silence_flips_available() {
    let (mosquitto, mut sup, observer) = setup().await;
    let state_sub = observer
        .declare_subscriber(AVAILABLE_KEY)
        .await
        .expect("state subscriber");
    let event_sub = observer
        .declare_subscriber(EVENT_KEY)
        .await
        .expect("event subscriber");
    let mut mqtt = Mqtt::connect(mosquitto.port, "test-availability").await;

    mqtt.publish(&format!("{BASE}/ivt490/state/serial/GT1"), "21.50").await;
    expect_states(&state_sub, &[(AVAILABLE_KEY, json!(true))]).await;

    // Nothing more from the device: the receive timer runs out.
    expect_states(&state_sub, &[(AVAILABLE_KEY, json!(false))]).await;
    expect_event_kind(&event_sub, "device-silent").await;

    mqtt.publish(&format!("{BASE}/ivt490/state/serial/GT1"), "21.60").await;
    expect_states(&state_sub, &[(AVAILABLE_KEY, json!(true))]).await;

    sup.shutdown();
}

/// (b)+(f) An arbitrated manual-band setpoint wish forwards through the
/// arbiter and reaches MQTT as a stringified float; while that manual
/// lease holds, a direct automation-band wish on the same key is refused
/// upstream and never reaches MQTT — the same structural proof as z2m's
/// lock test that this adapter has no home/cmd path of its own.
#[tokio::test(flavor = "multi_thread")]
async fn manual_setpoint_reaches_mqtt_via_arbiter_then_automation_refused() {
    let (mosquitto, mut sup, observer) = setup().await;
    let mut arbiter_watch = health_watch(&observer, "arbiter").await;
    await_health(&mut arbiter_watch, Duration::from_secs(60), |h| {
        h.status == HealthStatus::Running
    })
    .await;

    let mut mqtt = Mqtt::connect(mosquitto.port, "test-setpoint-cmd").await;
    mqtt.subscribe(&format!("{BASE}/controller/set/+")).await;

    let manual_wish = json!({"value": 21.5, "priority": "manual", "actor": "test"});
    observer
        .put(SETPOINT_CMD_KEY, manual_wish.to_string())
        .await
        .expect("cmd put");
    let (topic, payload) = mqtt
        .next_message(Duration::from_secs(10))
        .await
        .expect("set publish for setpoint command");
    assert_eq!(topic, SETPOINT_SET_TOPIC);
    assert_eq!(payload, b"21.5");

    // The manual lease is now held (hold_minutes = 30 in the fixture): a
    // direct automation-band wish on the same key is refused upstream.
    let auto_wish = json!({"value": 18.0, "priority": "automation", "actor": "scheduler"});
    observer
        .put(SETPOINT_CMD_KEY, auto_wish.to_string())
        .await
        .expect("cmd put");
    let silence = mqtt.next_message(Duration::from_millis(1500)).await;
    assert!(silence.is_none(), "refused automation wish reached MQTT: {silence:?}");

    sup.shutdown();
}

/// (b2) A fed input (docs/design.md, Device feeds): the fixture wires the
/// heat pump's indoor_temperature_actual to the fusion's virtual sensor.
/// Each source sample is forwarded to the device's set topic as a float,
/// not retained; while the source is unavailable nothing is forwarded and
/// the topic's retained slot is cleared once; a command naming the fed
/// input is refused, since a fed input has one master.
#[tokio::test(flavor = "multi_thread")]
async fn fed_input_follows_its_source_and_stops_on_loss() {
    let (mosquitto, mut sup, observer) = setup().await;
    let mut mqtt = Mqtt::connect(mosquitto.port, "test-feed").await;
    mqtt.subscribe(&format!("{BASE}/controller/set/+")).await;
    let event_sub = observer
        .declare_subscriber(EVENT_KEY)
        .await
        .expect("event subscriber");

    // A source sample forwards, as a float.
    observer.put(FEED_SOURCE_KEY, "21.3").await.expect("state put");
    let (topic, payload) = mqtt
        .next_message(Duration::from_secs(10))
        .await
        .expect("feed forwarded to the set topic");
    assert_eq!(topic, FEED_SET_TOPIC);
    assert_eq!(payload, b"21.3");

    // Not retained: a fresh subscriber sees nothing.
    let mut late = Mqtt::connect(mosquitto.port, "test-feed-late").await;
    late.subscribe(FEED_SET_TOPIC).await;
    let retained = late.next_message(Duration::from_millis(1000)).await;
    assert!(retained.is_none(), "a fed value must not be retained: {retained:?}");

    // Source goes unavailable: the retained slot is cleared once (an empty
    // retained publish) and a following sample is not forwarded.
    observer
        .put(FEED_SOURCE_AVAILABLE_KEY, "false")
        .await
        .expect("available put");
    let (topic, payload) = mqtt
        .next_message(Duration::from_secs(10))
        .await
        .expect("retained slot cleared on source loss");
    assert_eq!((topic.as_str(), payload.as_slice()), (FEED_SET_TOPIC, &b""[..]));
    expect_event_kind(&event_sub, "feed-source-lost").await;
    observer.put(FEED_SOURCE_KEY, "22.0").await.expect("state put");
    let silence = mqtt.next_message(Duration::from_millis(1500)).await;
    assert!(silence.is_none(), "sample forwarded while source unavailable: {silence:?}");

    // Source returns: forwarding resumes.
    observer
        .put(FEED_SOURCE_AVAILABLE_KEY, "true")
        .await
        .expect("available put");
    observer.put(FEED_SOURCE_KEY, "22.5").await.expect("state put");
    let (topic, payload) = mqtt
        .next_message(Duration::from_secs(10))
        .await
        .expect("feed resumes with the source");
    assert_eq!((topic.as_str(), payload.as_slice()), (FEED_SET_TOPIC, &b"22.5"[..]));

    // One master: a command naming the fed input drops with invalid-command
    // and nothing reaches MQTT. (indoor_temperature_actual was never a
    // command aspect; outdoor_temperature_offset would be, and is wired
    // the same way at the reporting house.)
    let wish = json!({"value": 20.0, "priority": "manual", "actor": "test"});
    observer
        .put(
            "home/arbiter/utility/heatpump/indoor_temperature_actual",
            wish.to_string(),
        )
        .await
        .expect("arbiter put");
    expect_drop_event(&event_sub, "invalid-command").await;
    let silence = mqtt.next_message(Duration::from_millis(1000)).await;
    assert!(silence.is_none(), "command on a fed input reached MQTT: {silence:?}");

    sup.shutdown();
}

/// (c) An out-of-range setpoint drops with an invalid-command health event
/// carrying the offending aspect and value, and nothing reaches MQTT.
#[tokio::test(flavor = "multi_thread")]
async fn out_of_range_setpoint_drops_with_invalid_command_event() {
    let (mosquitto, mut sup, observer) = setup().await;
    let mut arbiter_watch = health_watch(&observer, "arbiter").await;
    await_health(&mut arbiter_watch, Duration::from_secs(60), |h| {
        h.status == HealthStatus::Running
    })
    .await;

    let event_sub = observer
        .declare_subscriber(EVENT_KEY)
        .await
        .expect("event subscriber");
    let mut mqtt = Mqtt::connect(mosquitto.port, "test-out-of-range").await;
    mqtt.subscribe(&format!("{BASE}/controller/set/+")).await;

    // Bounds are 10-30 degC; 35.0 is out of range.
    let wish = json!({"value": 35.0, "priority": "manual", "actor": "test"});
    observer.put(SETPOINT_CMD_KEY, wish.to_string()).await.expect("cmd put");

    let event = expect_drop_event(&event_sub, "invalid-command").await;
    assert_eq!(event["aspect"], json!("setpoint"));
    assert_eq!(event["value"], json!(35.0));

    let silence = mqtt.next_message(Duration::from_millis(1500)).await;
    assert!(silence.is_none(), "out-of-range setpoint reached MQTT: {silence:?}");

    sup.shutdown();
}

/// (c') operating_mode is strictly the integer 1, 2 or 3: a valid mode
/// rides the arbiter to MQTT as an integer string, an out-of-enum integer
/// and a non-integer both drop with the invalid-command event and never
/// reach MQTT. (Manual band throughout: equal bands pass the arbiter, so
/// the drops observed are the adapter's own validation, not refusals.)
#[tokio::test(flavor = "multi_thread")]
async fn operating_mode_enum_enforced() {
    let (mosquitto, mut sup, observer) = setup().await;
    let mut arbiter_watch = health_watch(&observer, "arbiter").await;
    await_health(&mut arbiter_watch, Duration::from_secs(60), |h| {
        h.status == HealthStatus::Running
    })
    .await;

    let event_sub = observer
        .declare_subscriber(EVENT_KEY)
        .await
        .expect("event subscriber");
    let mut mqtt = Mqtt::connect(mosquitto.port, "test-operating-mode").await;
    mqtt.subscribe(&format!("{BASE}/controller/set/+")).await;

    let cmd_key = "home/cmd/utility/heatpump/operating_mode";

    // Mode 2 (BLOCK) forwards and lands as the integer string "2".
    let wish = json!({"value": 2, "priority": "manual", "actor": "test"});
    observer.put(cmd_key, wish.to_string()).await.expect("cmd put");
    let (topic, payload) = mqtt
        .next_message(Duration::from_secs(10))
        .await
        .expect("set publish for operating_mode command");
    assert_eq!(topic, format!("{BASE}/controller/set/operating_mode"));
    assert_eq!(payload, b"2");

    // 4 is outside the 1/2/3 enum: drop with event, nothing on MQTT.
    let wish = json!({"value": 4, "priority": "manual", "actor": "test"});
    observer.put(cmd_key, wish.to_string()).await.expect("cmd put");
    let event = expect_drop_event(&event_sub, "invalid-command").await;
    assert_eq!(event["aspect"], json!("operating_mode"));
    assert_eq!(event["value"], json!(4));
    let silence = mqtt.next_message(Duration::from_millis(1500)).await;
    assert!(silence.is_none(), "out-of-enum operating_mode reached MQTT: {silence:?}");

    // A non-integer drops the same way.
    let wish = json!({"value": 2.5, "priority": "manual", "actor": "test"});
    observer.put(cmd_key, wish.to_string()).await.expect("cmd put");
    let event = expect_drop_event(&event_sub, "invalid-command").await;
    assert_eq!(event["aspect"], json!("operating_mode"));
    assert_eq!(event["value"], json!(2.5));
    let silence = mqtt.next_message(Duration::from_millis(1500)).await;
    assert!(silence.is_none(), "non-integer operating_mode reached MQTT: {silence:?}");

    sup.shutdown();
}

/// (d) THE CONTRACT: every arbiter-forwarded payload is an envelope
/// `{value, priority, actor}`. A bare value with no envelope, published
/// directly on the arbiter-class key the adapter actually subscribes to
/// (this entity is arbitrated: it has no home/cmd path of its own), is
/// dropped with a health event instead of reaching MQTT; a properly
/// enveloped command on the same key still works afterwards.
#[tokio::test(flavor = "multi_thread")]
async fn envelope_less_command_drops_with_health_event() {
    let (mosquitto, mut sup, observer) = setup().await;
    let event_sub = observer
        .declare_subscriber(EVENT_KEY)
        .await
        .expect("event subscriber");
    let mut mqtt = Mqtt::connect(mosquitto.port, "test-no-envelope").await;
    mqtt.subscribe(&format!("{BASE}/controller/set/+")).await;

    observer.put(SETPOINT_ARBITER_KEY, "21.0").await.expect("cmd put");
    expect_drop_event(&event_sub, "invalid-command").await;
    let silence = mqtt.next_message(Duration::from_millis(1500)).await;
    assert!(silence.is_none(), "envelope-less command reached MQTT: {silence:?}");

    let envelope = json!({"value": 21.0, "priority": "manual", "actor": "test"});
    observer
        .put(SETPOINT_ARBITER_KEY, envelope.to_string())
        .await
        .expect("cmd put");
    let (topic, payload) = mqtt
        .next_message(Duration::from_secs(10))
        .await
        .expect("set publish for enveloped command");
    assert_eq!(topic, SETPOINT_SET_TOPIC);
    assert_eq!(payload, b"21.0");

    sup.shutdown();
}

/// (e) The adapter honors the step-2 unit contract: liveliness token when
/// ready, clean SIGTERM shutdown within the grace, no orphans.
#[tokio::test(flavor = "multi_thread")]
async fn adapter_honors_unit_contract() {
    let (_mosquitto, mut sup, observer) = setup().await;
    let mut watch = health_watch(&observer, "ivt490").await;
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
