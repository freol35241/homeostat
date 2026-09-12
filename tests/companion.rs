//! Companion adapter integration tests: each scenario spawns a real
//! mosquitto broker on a free port plus the real supervisor on the
//! companion fixture house, and asserts on both buses — the phone's MQTT
//! subtree and the house bus.

mod common;

use std::time::Duration;

use serde_json::{json, Value};
use zenoh::sample::SampleKind;

use common::{
    assert_unit_contract, await_discovery, await_states, cache_read, expect_drop_event,
    expect_states, matched_publisher, next_event, Mosquitto, Mqtt, StateSub, Supervisor,
};

const FIXTURE: &str = "tests/fixture_house_companion";
const PORT_ENV: &str = "HOMEOSTAT_TEST_MQTT_PORT";
const EVENT_KEY: &str = "home/health/companion/event";
const ALICE_MESSAGE: &str = "home/cmd/person/alice_phone/message";
const ALICE_ALERT: &str = "home/cmd/person/alice_phone/alert";

/// Spawns broker + supervisor on the fixture and waits for the adapter's
/// liveliness token (generous timeout: first run resolves the uv env).
async fn setup() -> (Mosquitto, Supervisor, zenoh::Session) {
    let mosquitto = Mosquitto::spawn();
    let port = mosquitto.port.to_string();
    let sup = Supervisor::spawn_with_env(FIXTURE, &[(PORT_ENV, &port)]);
    let observer = sup.observer().await;
    let token_sub = observer
        .liveliness()
        .declare_subscriber("home/health/companion/alive")
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

async fn event_sub(observer: &zenoh::Session) -> StateSub {
    observer
        .declare_subscriber(EVENT_KEY)
        .await
        .expect("event subscriber")
}

fn wish(text: Value, actor: &str) -> Value {
    json!({"value": text, "priority": "automation", "actor": actor})
}

fn body(payload: &[u8]) -> Value {
    serde_json::from_slice(payload).expect("the phone's payload is JSON")
}

/// Polls the core mirror until `key` carries a number, and returns it —
/// the shape for a timestamp no test can predict.
async fn await_number(observer: &zenoh::Session, key: &str) -> f64 {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(value) = cache_read(observer, key).await {
            if let Some(number) = value.as_f64() {
                return number;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{key} never carried a number"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// (a) The phone's two upward channels: a geofence transition becomes
/// `presence` on the person, and an opt-in position fix fans out to the
/// scalar aspects — the optional fields only when the fix carries them.
#[tokio::test(flavor = "multi_thread")]
async fn presence_and_position_translate_to_bus_state() {
    let (mosquitto, mut sup, observer) = setup().await;
    let state_sub = observer
        .declare_subscriber("home/state/**")
        .await
        .expect("state subscriber");
    let mut mqtt = Mqtt::connect(mosquitto.port, "test-state").await;

    mqtt.publish("companion/alice/person/presence", "true")
        .await;
    expect_states(
        &state_sub,
        &[("home/state/person/alice/presence", json!(true))],
    )
    .await;

    mqtt.publish(
        "companion/alice/person/position",
        r#"{"lat":59.33,"lon":18.06,"accuracy":12,"battery":87,"fixed_at":1752600000}"#,
    )
    .await;
    expect_states(
        &state_sub,
        &[
            ("home/state/person/alice/lat", json!(59.33)),
            ("home/state/person/alice/lon", json!(18.06)),
            ("home/state/person/alice/accuracy", json!(12)),
            ("home/state/person/alice/battery", json!(87)),
            ("home/state/person/alice/fixed_at", json!(1752600000)),
        ],
    )
    .await;

    // A fix without the optional fields publishes lat/lon and invents
    // nothing: the next presence sentinel arrives with no accuracy
    // between it and the fix.
    mqtt.publish(
        "companion/alice/person/position",
        r#"{"lat":1.0,"lon":2.0}"#,
    )
    .await;
    mqtt.publish("companion/alice/person/presence", "false")
        .await;
    expect_states(
        &state_sub,
        &[
            ("home/state/person/alice/lat", json!(1.0)),
            ("home/state/person/alice/presence", json!(false)),
        ],
    )
    .await;

    assert_unit_contract(&mut sup, &observer, "companion").await;
}

/// (b) A message and an alert reach the phone's own notifier topics,
/// carrying the envelope's actor, and the broker's QoS 1 ack publishes as
/// `delivered` — the delivery path took it, never a human's receipt.
#[tokio::test(flavor = "multi_thread")]
async fn notifications_reach_the_phone_and_publish_delivered() {
    let (mosquitto, mut sup, observer) = setup().await;
    let mut mqtt = Mqtt::connect(mosquitto.port, "test-notify").await;
    mqtt.subscribe("companion/alice/notifier/#").await;

    let message = matched_publisher(&observer, ALICE_MESSAGE).await;
    message
        .put(wish(json!("Irrigation skipped: it rained"), "irrigation").to_string())
        .await
        .expect("put");
    let (topic, payload) = mqtt
        .next_message(Duration::from_secs(20))
        .await
        .expect("message on the phone's topic");
    assert_eq!(topic, "companion/alice/notifier/message");
    let sent = body(&payload);
    assert_eq!(sent["text"], "Irrigation skipped: it rained");
    assert_eq!(sent["actor"], "irrigation");
    assert!(sent["sent_at"].is_number(), "{sent}");

    let alert = matched_publisher(&observer, ALICE_ALERT).await;
    alert
        .put(wish(json!("Motion in the hall and nobody home"), "intrusion").to_string())
        .await
        .expect("put");
    let (topic, payload) = mqtt
        .next_message(Duration::from_secs(20))
        .await
        .expect("alert on the phone's topic");
    assert_eq!(topic, "companion/alice/notifier/alert");
    assert_eq!(body(&payload)["text"], "Motion in the hall and nobody home");

    // delivered is the broker's ack, so it lands without the phone ever
    // saying anything back.
    let delivered = await_number(&observer, "home/state/person/alice_phone/delivered").await;
    assert!(delivered > 0.0, "delivered is an epoch: {delivered}");

    assert_unit_contract(&mut sup, &observer, "companion").await;
}

/// (c) The far-end receipt and the phone's own liveness: an ack becomes
/// `acknowledged` on the notifier, and the app's birth/last-will topic
/// flips `available` on BOTH of that phone's entities, on transition.
#[tokio::test(flavor = "multi_thread")]
async fn ack_and_availability_reach_both_faces() {
    let (mosquitto, mut sup, observer) = setup().await;
    let state_sub = observer
        .declare_subscriber("home/state/**")
        .await
        .expect("state subscriber");
    let mut mqtt = Mqtt::connect(mosquitto.port, "test-ack").await;

    mqtt.publish("companion/alice/notifier/ack", "1752600123")
        .await;
    expect_states(
        &state_sub,
        &[(
            "home/state/person/alice_phone/acknowledged",
            json!(1752600123),
        )],
    )
    .await;

    mqtt.publish("companion/alice/available", "true").await;
    await_states(
        &observer,
        &state_sub,
        &[
            ("home/state/person/alice/available", json!(true)),
            ("home/state/person/alice_phone/available", json!(true)),
        ],
    )
    .await;

    // What a last will looks like on the wire when the phone drops off.
    mqtt.publish("companion/alice/available", "false").await;
    await_states(
        &observer,
        &state_sub,
        &[
            ("home/state/person/alice/available", json!(false)),
            ("home/state/person/alice_phone/available", json!(false)),
        ],
    )
    .await;

    assert_unit_contract(&mut sup, &observer, "companion").await;
}

/// (d) Everything the phone can say wrong drops with a health event and
/// leaves the adapter translating.
#[tokio::test(flavor = "multi_thread")]
async fn bad_phone_input_drops_with_health_event() {
    let (mosquitto, mut sup, observer) = setup().await;
    let events = event_sub(&observer).await;
    let state_sub = observer
        .declare_subscriber("home/state/**")
        .await
        .expect("state subscriber");
    let mut mqtt = Mqtt::connect(mosquitto.port, "test-bad").await;

    mqtt.publish("companion/alice/person/presence", "certainly not json")
        .await;
    expect_drop_event(&events, "malformed-payload").await;

    // A presence that is not a boolean, a fix without lat, an ack that is
    // not a number, an availability that is not a boolean.
    mqtt.publish("companion/alice/person/presence", r#""home""#)
        .await;
    expect_drop_event(&events, "malformed-payload").await;
    mqtt.publish("companion/alice/person/position", r#"{"lon":18.06}"#)
        .await;
    expect_drop_event(&events, "malformed-payload").await;
    mqtt.publish("companion/alice/notifier/ack", r#""just now""#)
        .await;
    expect_drop_event(&events, "malformed-payload").await;
    mqtt.publish("companion/alice/available", r#""yes""#).await;
    expect_drop_event(&events, "malformed-payload").await;

    // Still translating.
    mqtt.publish("companion/alice/person/presence", "true")
        .await;
    expect_states(
        &state_sub,
        &[("home/state/person/alice/presence", json!(true))],
    )
    .await;

    sup.shutdown();
    drop(mosquitto);
}

/// (e) A subtree no entity file binds reports once, not per publish: an
/// unbound phone keeps publishing forever and discovery is not the health
/// feed's job.
#[tokio::test(flavor = "multi_thread")]
async fn an_unbound_phone_reports_once() {
    let (mosquitto, mut sup, observer) = setup().await;
    let events = event_sub(&observer).await;
    let mut mqtt = Mqtt::connect(mosquitto.port, "test-unbound").await;

    for _ in 0..3 {
        mqtt.publish("companion/zoe/person/presence", "true").await;
    }
    let event = next_event(&events).await;
    assert_eq!(event["reason"], json!("unknown-device"));
    assert_eq!(event["topic"], json!("companion/zoe/person/presence"));

    // A sentinel published last, asserted strictly: any further
    // unknown-device from zoe's remaining two publishes would precede it.
    mqtt.publish("companion/alice/person/presence", "not json")
        .await;
    assert_eq!(
        next_event(&events).await["reason"],
        json!("malformed-payload"),
        "an unbound phone must not report every publish"
    );

    sup.shutdown();
    drop(mosquitto);
}

/// (f) What never reaches the phone: a non-string value, an empty string,
/// an unknown aspect, an envelope-less payload.
#[tokio::test(flavor = "multi_thread")]
async fn invalid_commands_never_reach_the_phone() {
    let (mosquitto, mut sup, observer) = setup().await;
    let events = event_sub(&observer).await;
    let mut mqtt = Mqtt::connect(mosquitto.port, "test-invalid").await;
    mqtt.subscribe("companion/alice/notifier/#").await;

    let alert = matched_publisher(&observer, ALICE_ALERT).await;
    alert
        .put(wish(json!(42), "x").to_string())
        .await
        .expect("put");
    expect_drop_event(&events, "invalid-command").await;
    alert
        .put(wish(json!("   "), "x").to_string())
        .await
        .expect("put");
    expect_drop_event(&events, "invalid-command").await;
    alert
        .put(json!("bare string, no envelope").to_string())
        .await
        .expect("put");
    expect_drop_event(&events, "invalid-command").await;

    let odd = matched_publisher(&observer, "home/cmd/person/alice_phone/ring").await;
    odd.put(wish(json!("hello"), "x").to_string())
        .await
        .expect("put");
    let event = expect_drop_event(&events, "invalid-command").await;
    assert_eq!(event["aspect"], "ring");

    // The sentinel is the first thing the phone hears.
    let message = matched_publisher(&observer, ALICE_MESSAGE).await;
    message
        .put(wish(json!("the only one"), "x").to_string())
        .await
        .expect("put");
    let (topic, payload) = mqtt
        .next_message(Duration::from_secs(20))
        .await
        .expect("the sentinel");
    assert_eq!(topic, "companion/alice/notifier/message");
    assert_eq!(body(&payload)["text"], "the only one");

    sup.shutdown();
    drop(mosquitto);
}

/// (g) Discovery: one record per bound entity, each with the aspect
/// descriptor its face needs.
#[tokio::test(flavor = "multi_thread")]
async fn discovery_describes_both_faces() {
    let (_mosquitto, mut sup, observer) = setup().await;
    let doc = await_discovery(&observer, "companion", |d| {
        d.as_array().is_some_and(|records| records.len() == 2)
    })
    .await;
    let records = doc.as_array().expect("discovery is an array");

    let person = records
        .iter()
        .find(|r| r["id"] == json!("alice/person"))
        .expect("person record");
    assert_eq!(person["configured"], json!(true));
    assert_eq!(person["entity"], json!("alice"));
    assert_eq!(
        person["suggested"],
        json!({"capability": "person", "features": []})
    );
    assert_eq!(person["aspects"]["fields"]["presence"]["kind"], "boolean");

    let notifier = records
        .iter()
        .find(|r| r["id"] == json!("alice/notifier"))
        .expect("notifier record");
    assert_eq!(notifier["entity"], json!("alice_phone"));
    assert_eq!(
        notifier["suggested"],
        json!({"capability": "notifier", "features": ["alert", "acknowledged"]})
    );
    let fields = &notifier["aspects"]["fields"];
    assert_eq!(fields["delivered"]["kind"], "number");
    assert_eq!(fields["acknowledged"]["kind"], "number");

    sup.shutdown();
}
