//! Aduro pellet-burner adapter integration tests: each scenario spawns a
//! real mosquitto broker on a free port plus the real supervisor on the
//! aduro fixture house, and asserts on both buses.

mod common;

use std::time::Duration;

use homeostat::bus::HealthStatus;
use serde_json::json;
use zenoh::sample::SampleKind;

use common::{
    assert_unit_contract, await_health, expect_drop_event, expect_event_kind, expect_states,
    health_watch, Mosquitto, Mqtt, Supervisor,
};

const FIXTURE: &str = "tests/fixture_house_aduro";
const PORT_ENV: &str = "HOMEOSTAT_TEST_MQTT_PORT";
const BASE: &str = "aduro2mqtt"; // the fixture entity's base topic (its `id`)
const EVENT_KEY: &str = "home/health/aduro/event";
const ON_CMD_KEY: &str = "home/cmd/livingroom/burner/on";
const ON_ARBITER_KEY: &str = "home/arbiter/livingroom/burner/on";
const POWER_CMD_KEY: &str = "home/cmd/livingroom/burner/power_level";
const SET_TOPIC: &str = "aduro2mqtt/set";
const AVAILABLE_KEY: &str = "home/state/livingroom/burner/available";

/// A status document the bridge would publish: the burner idle (state 14)
/// at 10% fixed power, with the readings the vocabulary normalizes and a
/// few passthrough fields, floated as the bridge floats them.
fn status(smoke_temp: f64) -> String {
    json!({
        "boiler_temp": 21.4,
        "smoke_temp": smoke_temp,
        "shaft_temp": 23.1,
        "state": 14.0,
        "substate": 0.0,
        "power_pct": 0.0,
        "regulation.fixed_power": 10.0,
        "city": "Somewhere",
    })
    .to_string()
}

async fn setup() -> (Mosquitto, Supervisor, zenoh::Session) {
    let mosquitto = Mosquitto::spawn();
    let port = mosquitto.port.to_string();
    let sup = Supervisor::spawn_with_env(FIXTURE, &[(PORT_ENV, &port)]);
    let observer = sup.observer().await;
    let token_sub = observer
        .liveliness()
        .declare_subscriber("home/health/aduro/alive")
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

/// Waits for the arbiter to be running: commands to the arbitrated burner
/// only reach the adapter through it.
async fn await_arbiter(observer: &zenoh::Session) {
    let mut watch = health_watch(observer, "arbiter").await;
    await_health(&mut watch, Duration::from_secs(60), |h| {
        h.status == HealthStatus::Running
    })
    .await;
}

/// (a) One status document fans out to the burner vocabulary (`on` derived
/// from the run state, `power_level` as an int, the two temperatures
/// normalized) plus passthrough under firmware names, dotted or not; an
/// operating document publishes prefixed; the identical republish a poll
/// later yields NOTHING, a changed field yields exactly that field; a
/// non-object payload drops with malformed-payload; and the discovery
/// record carries the descriptor.
#[tokio::test(flavor = "multi_thread")]
async fn status_translates_on_change_only() {
    let (mosquitto, mut sup, observer) = setup().await;
    let state_sub = observer
        .declare_subscriber("home/state/livingroom/burner/*")
        .await
        .expect("state subscriber");
    let event_sub = observer
        .declare_subscriber(EVENT_KEY)
        .await
        .expect("event subscriber");
    let mut mqtt = Mqtt::connect(mosquitto.port, "test-state").await;

    mqtt.publish(&format!("{BASE}/status"), &status(25.5)).await;
    mqtt.publish(&format!("{BASE}/operating"), r#"{"boiler_temp": 21.4}"#)
        .await;
    // Unsubscribed topics: nothing, not even an event.
    mqtt.publish(
        &format!("{BASE}/settings/regulation"),
        r#"{"fixed_power": 10.0}"#,
    )
    .await;
    mqtt.publish(&format!("{BASE}/consumption/counter"), "[1.0, 2.0]")
        .await;

    expect_states(
        &state_sub,
        &[
            ("home/state/livingroom/burner/available", json!(true)),
            ("home/state/livingroom/burner/on", json!(false)),
            ("home/state/livingroom/burner/power_level", json!(10)),
            ("home/state/livingroom/burner/flue_temperature", json!(25.5)),
            (
                "home/state/livingroom/burner/boiler_temperature",
                json!(21.4),
            ),
            ("home/state/livingroom/burner/shaft_temp", json!(23.1)),
            ("home/state/livingroom/burner/state", json!(14.0)),
            ("home/state/livingroom/burner/city", json!("Somewhere")),
            (
                "home/state/livingroom/burner/operating_boiler_temp",
                json!(21.4),
            ),
        ],
    )
    .await;

    // Drain: the first poll may still be delivering; then republish the
    // same document and require silence.
    while tokio::time::timeout(Duration::from_millis(500), state_sub.recv_async())
        .await
        .is_ok()
    {}
    mqtt.publish(&format!("{BASE}/status"), &status(25.5)).await;
    let silence = tokio::time::timeout(Duration::from_millis(1500), state_sub.recv_async()).await;
    assert!(
        silence.is_err(),
        "unchanged status republished to the bus: {silence:?}"
    );

    // One field moves: exactly one sample.
    mqtt.publish(&format!("{BASE}/status"), &status(26.0)).await;
    let sample = tokio::time::timeout(Duration::from_secs(10), state_sub.recv_async())
        .await
        .expect("changed field within 10s")
        .expect("state stream open");
    assert_eq!(
        sample.key_expr().as_str(),
        "home/state/livingroom/burner/flue_temperature"
    );
    let silence = tokio::time::timeout(Duration::from_millis(1500), state_sub.recv_async()).await;
    assert!(
        silence.is_err(),
        "unchanged fields republished: {silence:?}"
    );

    let no_event = tokio::time::timeout(Duration::from_millis(500), event_sub.recv_async()).await;
    assert!(no_event.is_err(), "unexpected health event: {no_event:?}");

    // A non-object payload is malformed.
    mqtt.publish(&format!("{BASE}/status"), "[1, 2, 3]").await;
    expect_drop_event(&event_sub, "malformed-payload").await;

    let replies = observer
        .get("home/discovery/aduro")
        .await
        .expect("discovery query");
    let mut inventory = None;
    while let Ok(reply) = replies.recv_async().await {
        if let Ok(sample) = reply.result() {
            inventory =
                serde_json::from_slice::<serde_json::Value>(&sample.payload().to_bytes()).ok();
        }
    }
    let inventory = inventory.expect("discovery inventory served");
    let record = &inventory[0];
    assert_eq!(record["entity"], json!("burner"), "{inventory}");
    assert_eq!(record["id"], json!(BASE));
    assert_eq!(record["bound"], json!(true));
    assert_eq!(
        record["suggested"],
        json!({"capability": "burner", "features": ["power_level"]})
    );
    let fields = &record["aspects"]["fields"];
    assert_eq!(
        record["aspects"]["groups"],
        json!(["control", "readings", "status"])
    );
    assert_eq!(
        fields["on"]["command"],
        json!({"type": "enum", "editable_by": "family"})
    );
    assert_eq!(
        fields["on"]["values"][1],
        json!({"value": true, "label": "on"})
    );
    assert_eq!(
        fields["power_level"]["values"][1],
        json!({"value": 50, "label": "50%"})
    );
    assert_eq!(
        fields["flue_temperature"]["label"],
        json!("flue (smoke_temp)")
    );
    assert!(
        fields.get("city").is_none(),
        "undescribed aspects are simply absent"
    );

    sup.shutdown();
}

/// (a2) Receive-timer availability: a status message flips available =
/// true, availability_timeout_s (5 s in the fixture) of silence flips it
/// false with a "device-silent" health event, the next message flips it
/// back.
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

    mqtt.publish(&format!("{BASE}/status"), &status(25.5)).await;
    expect_states(&state_sub, &[(AVAILABLE_KEY, json!(true))]).await;

    expect_states(&state_sub, &[(AVAILABLE_KEY, json!(false))]).await;
    expect_event_kind(&event_sub, "device-silent").await;

    mqtt.publish(&format!("{BASE}/status"), &status(25.5)).await;
    expect_states(&state_sub, &[(AVAILABLE_KEY, json!(true))]).await;

    sup.shutdown();
}

/// (b) Manual-band `on` and `power_level` wishes ride the arbiter to
/// {base}/set in the bridge's own command shape; while the manual `on`
/// lease holds, an automation-band `on` is refused upstream — but the
/// automation's `power_level` still passes, because leases are per aspect.
#[tokio::test(flavor = "multi_thread")]
async fn commands_reach_set_topic_via_arbiter_per_aspect() {
    let (mosquitto, mut sup, observer) = setup().await;
    await_arbiter(&observer).await;
    let mut mqtt = Mqtt::connect(mosquitto.port, "test-cmd").await;
    mqtt.subscribe(SET_TOPIC).await;

    let wish = json!({"value": true, "priority": "manual", "actor": "test"});
    observer
        .put(ON_CMD_KEY, wish.to_string())
        .await
        .expect("cmd put");
    let (topic, payload) = mqtt
        .next_message(Duration::from_secs(10))
        .await
        .expect("start publish");
    assert_eq!(topic, SET_TOPIC);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&payload).unwrap(),
        json!({"path": "misc.start", "value": "1"})
    );

    let wish = json!({"value": false, "priority": "manual", "actor": "test"});
    observer
        .put(ON_CMD_KEY, wish.to_string())
        .await
        .expect("cmd put");
    let (_, payload) = mqtt
        .next_message(Duration::from_secs(10))
        .await
        .expect("stop publish");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&payload).unwrap(),
        json!({"path": "misc.stop", "value": "1"})
    );

    // The manual `on` lease holds (hold_minutes = 30): an automation `on`
    // is refused upstream and never reaches MQTT.
    let wish = json!({"value": true, "priority": "automation", "actor": "heat_plan"});
    observer
        .put(ON_CMD_KEY, wish.to_string())
        .await
        .expect("cmd put");
    let silence = mqtt.next_message(Duration::from_millis(1500)).await;
    assert!(
        silence.is_none(),
        "refused automation wish reached MQTT: {silence:?}"
    );

    // Per-aspect leases: the same automation's power_level passes.
    let wish = json!({"value": 50, "priority": "automation", "actor": "heat_plan"});
    observer
        .put(POWER_CMD_KEY, wish.to_string())
        .await
        .expect("cmd put");
    let (_, payload) = mqtt
        .next_message(Duration::from_secs(10))
        .await
        .expect("power publish");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&payload).unwrap(),
        json!({"path": "regulation.fixed_power", "value": 50})
    );

    sup.shutdown();
}

/// (c) Validation: a non-bool `on`, an out-of-enum or non-integer
/// `power_level`, an unknown aspect and an envelope-less payload all drop
/// with the right event and nothing reaches {base}/set.
#[tokio::test(flavor = "multi_thread")]
async fn invalid_commands_drop_with_events() {
    let (mosquitto, mut sup, observer) = setup().await;
    await_arbiter(&observer).await;
    let event_sub = observer
        .declare_subscriber(EVENT_KEY)
        .await
        .expect("event subscriber");
    let mut mqtt = Mqtt::connect(mosquitto.port, "test-invalid").await;
    mqtt.subscribe(SET_TOPIC).await;

    let wish = json!({"value": 1, "priority": "manual", "actor": "test"});
    observer
        .put(ON_CMD_KEY, wish.to_string())
        .await
        .expect("cmd put");
    let event = expect_drop_event(&event_sub, "invalid-command").await;
    assert_eq!(event["aspect"], json!("on"));
    assert_eq!(event["value"], json!(1));

    for value in [json!(30), json!(50.5), json!("50"), json!(true)] {
        let wish = json!({"value": value, "priority": "manual", "actor": "test"});
        observer
            .put(POWER_CMD_KEY, wish.to_string())
            .await
            .expect("cmd put");
        let event = expect_drop_event(&event_sub, "invalid-command").await;
        assert_eq!(event["aspect"], json!("power_level"));
        assert_eq!(event["value"], value);
    }

    let wish = json!({"value": 200.0, "priority": "manual", "actor": "test"});
    observer
        .put("home/cmd/livingroom/burner/smoke_temp", wish.to_string())
        .await
        .expect("cmd put");
    let event = expect_drop_event(&event_sub, "invalid-command").await;
    assert_eq!(event["aspect"], json!("smoke_temp"));

    observer.put(ON_ARBITER_KEY, "true").await.expect("cmd put");
    expect_drop_event(&event_sub, "invalid-command").await;

    let silence = mqtt.next_message(Duration::from_millis(1500)).await;
    assert!(
        silence.is_none(),
        "an invalid command reached MQTT: {silence:?}"
    );

    sup.shutdown();
}

/// (e) The adapter honors the unit contract: liveliness token when ready,
/// clean SIGTERM shutdown within the grace, no orphans.
#[tokio::test(flavor = "multi_thread")]
async fn adapter_honors_unit_contract() {
    let (_mosquitto, mut sup, observer) = setup().await;
    assert_unit_contract(&mut sup, &observer, "aduro").await;
}
