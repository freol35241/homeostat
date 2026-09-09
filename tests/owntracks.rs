//! OwnTracks adapter integration tests: each scenario spawns a real
//! mosquitto broker on a free port plus the real supervisor on the
//! owntracks fixture house, and asserts on both buses.

mod common;

use std::time::Duration;

use serde_json::{json, Value};
use zenoh::sample::SampleKind;

use common::{
    assert_unit_contract, expect_drop_event, expect_states, Mosquitto, Mqtt, next_event, Supervisor,
};

const FIXTURE: &str = "tests/fixture_house_owntracks";
const PORT_ENV: &str = "HOMEOSTAT_TEST_MQTT_PORT";
const EVENT_KEY: &str = "home/health/owntracks/event";

/// Spawns broker + supervisor on the fixture and waits for the adapter's
/// liveliness token (generous timeout: first run resolves the uv env).
async fn setup() -> (Mosquitto, Supervisor, zenoh::Session) {
    let mosquitto = Mosquitto::spawn();
    let port = mosquitto.port.to_string();
    let sup = Supervisor::spawn_with_env(FIXTURE, &[(PORT_ENV, &port)]);
    let observer = sup.observer().await;
    let token_sub = observer
        .liveliness()
        .declare_subscriber("home/health/owntracks/alive")
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

/// (a) A location fix translates to the scalar per-aspect state keys under
/// the reserved "person" pseudo-room.
#[tokio::test(flavor = "multi_thread")]
async fn location_translates_to_bus_state() {
    let (mosquitto, mut sup, observer) = setup().await;
    let state_sub = observer
        .declare_subscriber("home/state/**")
        .await
        .expect("state subscriber");
    let mut mqtt = Mqtt::connect(mosquitto.port, "test-state").await;

    mqtt.publish(
        "owntracks/alice/phone",
        r#"{"_type":"location","lat":59.33,"lon":18.06,"acc":12,"batt":87,"tst":1752600000}"#,
    )
    .await;
    expect_states(
        &state_sub,
        &[
            ("home/state/person/alice_phone/lat", json!(59.33)),
            ("home/state/person/alice_phone/lon", json!(18.06)),
            ("home/state/person/alice_phone/accuracy", json!(12)),
            ("home/state/person/alice_phone/battery", json!(87)),
            ("home/state/person/alice_phone/fixed_at", json!(1752600000)),
        ],
    )
    .await;

    sup.shutdown();
}

/// (b) Non-location `_type` payloads (transition, lwt, waypoint, ...) are
/// normal OwnTracks traffic: ignored silently, no health event, no state.
#[tokio::test(flavor = "multi_thread")]
async fn non_location_type_is_ignored() {
    let (mosquitto, mut sup, observer) = setup().await;
    let event_sub = observer
        .declare_subscriber(EVENT_KEY)
        .await
        .expect("event subscriber");
    let state_sub = observer
        .declare_subscriber("home/state/**")
        .await
        .expect("state subscriber");
    let mut mqtt = Mqtt::connect(mosquitto.port, "test-nonloc").await;

    mqtt.publish(
        "owntracks/alice/phone",
        r#"{"_type":"transition","event":"enter","desc":"home"}"#,
    )
    .await;

    // No event, no state within a generous window — just quiet.
    let no_event = tokio::time::timeout(Duration::from_millis(1500), event_sub.recv_async()).await;
    assert!(no_event.is_err(), "transition payload must not raise a health event");
    let no_state = tokio::time::timeout(Duration::from_millis(500), state_sub.recv_async()).await;
    assert!(no_state.is_err(), "transition payload must not publish state");

    // Still translating afterwards.
    mqtt.publish(
        "owntracks/alice/phone",
        r#"{"_type":"location","lat":1.0,"lon":2.0}"#,
    )
    .await;
    expect_states(
        &state_sub,
        &[
            ("home/state/person/alice_phone/lat", json!(1.0)),
            ("home/state/person/alice_phone/lon", json!(2.0)),
        ],
    )
    .await;

    sup.shutdown();
}

/// (c) Malformed JSON, a location fix missing lat/lon, and an unbound
/// device all drop with a health event without crashing the adapter.
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

    mqtt.publish("owntracks/alice/phone", "certainly not json").await;
    expect_drop_event(&event_sub, "malformed-payload").await;

    mqtt.publish("owntracks/alice/phone", r#"{"_type":"location","lat":1.0}"#).await;
    expect_drop_event(&event_sub, "malformed-payload").await;

    mqtt.publish(
        "owntracks/bob/phone",
        r#"{"_type":"location","lat":3.0,"lon":4.0}"#,
    )
    .await;
    expect_drop_event(&event_sub, "unknown-device").await;

    // Still alive and translating.
    mqtt.publish(
        "owntracks/alice/phone",
        r#"{"_type":"location","lat":5.0,"lon":6.0}"#,
    )
    .await;
    expect_states(
        &state_sub,
        &[
            ("home/state/person/alice_phone/lat", json!(5.0)),
            ("home/state/person/alice_phone/lon", json!(6.0)),
        ],
    )
    .await;

    sup.shutdown();
}

/// (d) The adapter honors the step-2 unit contract: liveliness token when
/// ready, clean SIGTERM shutdown within the grace, no orphans.
#[tokio::test(flavor = "multi_thread")]
async fn adapter_honors_unit_contract() {
    let (_mosquitto, mut sup, observer) = setup().await;
    assert_unit_contract(&mut sup, &observer, "owntracks").await;
}

/// (e) Discovery: every user/device pair seen on the broker — bound or
/// not — is tracked incrementally and republished at
/// home/discovery/owntracks.
#[tokio::test(flavor = "multi_thread")]
async fn seen_devices_published_as_discovery() {
    let (mosquitto, mut sup, observer) = setup().await;
    let mut mqtt = Mqtt::connect(mosquitto.port, "inventory-test").await;
    let sub = observer
        .declare_subscriber("home/discovery/owntracks")
        .await
        .expect("discovery subscriber");

    mqtt.publish(
        "owntracks/alice/phone",
        r#"{"_type":"location","lat":1.0,"lon":2.0}"#,
    )
    .await;
    let sample = tokio::time::timeout(Duration::from_secs(10), sub.recv_async())
        .await
        .expect("discovery document within 10s")
        .expect("discovery sample");
    let mut doc: Value =
        serde_json::from_slice(&sample.payload().to_bytes()).expect("discovery is JSON");
    assert_eq!(doc.as_array().expect("discovery is an array").len(), 1, "{doc}");

    // An unconfigured device shows up on its own traffic — the record set
    // grows and is republished, without disturbing the first record.
    mqtt.publish(
        "owntracks/bob/phone",
        r#"{"_type":"location","lat":3.0,"lon":4.0}"#,
    )
    .await;
    loop {
        let sample = tokio::time::timeout(Duration::from_secs(10), sub.recv_async())
            .await
            .expect("second discovery document within 10s")
            .expect("discovery sample");
        doc = serde_json::from_slice(&sample.payload().to_bytes()).expect("discovery is JSON");
        if doc.as_array().expect("discovery is an array").len() == 2 {
            break;
        }
    }

    let records = doc.as_array().expect("discovery is an array");
    let alice = records
        .iter()
        .find(|r| r["id"] == json!("alice/phone"))
        .expect("alice record");
    assert_eq!(alice["configured"], json!(true));
    assert_eq!(alice["entity"], json!("alice_phone"));
    assert_eq!(alice["suggested"], json!({"capability": "person", "features": []}));

    let bob = records
        .iter()
        .find(|r| r["id"] == json!("bob/phone"))
        .expect("bob record");
    assert_eq!(bob["configured"], json!(false));
    assert_eq!(bob["entity"], json!(null));
    assert_eq!(bob["suggested"], json!({"capability": "person", "features": []}));

    // The mirror serves it to late joiners.
    let replies = observer.get("home/discovery/owntracks").await.expect("get discovery");
    let reply = replies.recv_async().await.expect("mirrored discovery reply");
    let mirrored: Value = serde_json::from_slice(
        &reply.result().expect("mirrored sample").payload().to_bytes(),
    )
    .expect("mirrored discovery is JSON");
    assert_eq!(mirrored, doc, "mirror serves the same document");

    sup.shutdown();
}

/// An unbound phone keeps publishing forever, so the unknown-device event
/// fires once on first sight and then stays quiet — discovery already
/// carries the pair with configured=false.
#[tokio::test(flavor = "multi_thread")]
async fn an_unbound_phone_reports_once_not_per_fix() {
    let (mosquitto, mut sup, observer) = setup().await;
    let event_sub = observer
        .declare_subscriber(EVENT_KEY)
        .await
        .expect("event subscriber");
    let mut mqtt = Mqtt::connect(mosquitto.port, "test-unbound").await;

    for lat in [1.0, 2.0, 3.0] {
        mqtt.publish(
            "owntracks/carol/phone",
            &format!(r#"{{"_type":"location","lat":{lat},"lon":4.0}}"#),
        )
        .await;
    }
    assert_eq!(
        next_event(&event_sub).await["reason"],
        json!("unknown-device"),
        "first sight reports once"
    );

    // A sentinel published last, asserted strictly: any further
    // unknown-device from carol's remaining two fixes would precede it.
    mqtt.publish("owntracks/alice/phone", "certainly not json").await;
    assert_eq!(
        next_event(&event_sub).await["reason"],
        json!("malformed-payload"),
        "an unbound phone must not report every fix"
    );

    sup.shutdown();
    drop(mosquitto);
}
