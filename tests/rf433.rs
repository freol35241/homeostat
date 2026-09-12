//! RF433 adapter integration tests: each scenario spawns a real mosquitto
//! broker on a free port plus the real supervisor on the rf433 fixture
//! house, and asserts on both buses.
//!
//! The thing under test is mostly the DECAY. A one-way sender asserts and
//! never retracts, so every assertion here is really about when `false`
//! appears and when it does not.

mod common;

use std::time::Duration;

use serde_json::json;
use zenoh::sample::SampleKind;

use common::{await_mirror, expect_drop_event, expect_states, Mosquitto, Mqtt, Supervisor};

const FIXTURE: &str = "tests/fixture_house_rf433";
const PORT_ENV: &str = "HOMEOSTAT_TEST_MQTT_PORT";
const EVENTS: &str = "rf/SRFBtoMQTT";
const LWT: &str = "rf/LWT";
const EVENT_KEY: &str = "home/health/rf433/event";

// The fixture's bound codes.
const PIR: &str = "11970086";
const DOOR: &str = "13951014";
const SMOKE: &str = "10600462";

const PIR_KEY: &str = "home/state/hallway/hallway_motion/occupancy";
const DOOR_KEY: &str = "home/state/hallway/front_door/contact";
const SMOKE_KEY: &str = "home/state/landing/smoke_upstairs/smoke";

/// Spawns broker + supervisor on the fixture and waits for the adapter's
/// liveliness token (generous timeout: first run resolves the uv env).
async fn setup() -> (Mosquitto, Supervisor, zenoh::Session) {
    let mosquitto = Mosquitto::spawn();
    let port = mosquitto.port.to_string();
    let sup = Supervisor::spawn_with_env(FIXTURE, &[(PORT_ENV, &port)]);
    let observer = sup.observer().await;
    let token_sub = observer
        .liveliness()
        .declare_subscriber("home/health/rf433/alive")
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

/// (a) Every bound entity is `false` at startup, before any traffic. The
/// held value is the adapter's construct rather than a device reading, so
/// "nothing has asserted" is the honest state after a start — and it is
/// what makes a crash-looping adapter self-clear instead of leaving a
/// motion sensor stuck on.
#[tokio::test(flavor = "multi_thread")]
async fn every_bound_entity_starts_false() {
    let (_mosquitto, mut sup, observer) = setup().await;

    // The mirror, not a subscriber: these publishes happen before the
    // adapter is ready, so a test's subscriber never sees them. That is
    // also how a late-joining consumer sees them, which is the point.
    await_mirror(&observer, PIR_KEY, &json!(false)).await;
    await_mirror(&observer, DOOR_KEY, &json!(false)).await;
    await_mirror(&observer, SMOKE_KEY, &json!(false)).await;

    sup.shutdown();
}

/// (b) A burst asserts the bound entity, and the hold releases it. The
/// fixture's occupancy hold is 1 s, so both transitions land inside a test.
#[tokio::test(flavor = "multi_thread")]
async fn a_burst_asserts_and_the_hold_releases() {
    let (mosquitto, mut sup, observer) = setup().await;
    let state_sub = observer
        .declare_subscriber(PIR_KEY)
        .await
        .expect("state subscriber");
    let mut mqtt = Mqtt::connect(mosquitto.port, "test-burst").await;

    mqtt.publish(EVENTS, PIR).await;
    expect_states(&state_sub, &[(PIR_KEY, json!(true))]).await;
    // No republish needed: the sweeper releases it on its own.
    expect_states(&state_sub, &[(PIR_KEY, json!(false))]).await;

    sup.shutdown();
}

/// (c) Codes address entities independently: the door's code moves the
/// door, not the PIR, and the door's longer hold is still held when the
/// PIR's has already expired.
#[tokio::test(flavor = "multi_thread")]
async fn codes_address_their_own_entity() {
    let (mosquitto, mut sup, observer) = setup().await;
    let state_sub = observer
        .declare_subscriber("home/state/**")
        .await
        .expect("state subscriber");
    let mut mqtt = Mqtt::connect(mosquitto.port, "test-codes").await;

    mqtt.publish(EVENTS, DOOR).await;
    expect_states(&state_sub, &[(DOOR_KEY, json!(true))]).await;

    mqtt.publish(EVENTS, PIR).await;
    expect_states(&state_sub, &[(PIR_KEY, json!(true))]).await;

    // The PIR's 1 s hold expires while the door's much longer one stands.
    //
    // Note which instrument answers which question, because the adapter is
    // built on exactly this distinction: the PIR's release is an EVENT and
    // arrives on the subscriber, while "the door is still held" is STATE
    // and is read from the mirror. Waiting on a subscriber for the door
    // would wait forever — a held value is not republished, which is the
    // whole point of transitions-only.
    expect_states(&state_sub, &[(PIR_KEY, json!(false))]).await;
    await_mirror(&observer, DOOR_KEY, &json!(true)).await;

    sup.shutdown();
}

/// (d) The JSON payload current gateway firmware publishes carries the
/// same code under `value`.
///
/// ⚠️ This is the shape the adapter was written to from documentation, not
/// from a wire — the hardware available to its author speaks the legacy
/// bare-decimal form. The test pins the documented shape so a firmware that
/// disagrees fails here rather than in a house.
#[tokio::test(flavor = "multi_thread")]
async fn json_payloads_carry_the_code_in_value() {
    let (mosquitto, mut sup, observer) = setup().await;
    let state_sub = observer
        .declare_subscriber(SMOKE_KEY)
        .await
        .expect("state subscriber");
    let mut mqtt = Mqtt::connect(mosquitto.port, "test-json").await;

    mqtt.publish(
        EVENTS,
        &format!(r#"{{"raw":"A1B2C3","value":{SMOKE},"delay":9550}}"#),
    )
    .await;
    expect_states(&state_sub, &[(SMOKE_KEY, json!(true))]).await;

    sup.shutdown();
}

/// (e) An unbound code is not an error. A 433 MHz estate hears neighbours'
/// remotes, car keys and doorbells all day; discovery carries them so they
/// can be identified, and the health feed stays quiet.
#[tokio::test(flavor = "multi_thread")]
async fn unbound_codes_reach_discovery_not_the_health_feed() {
    let (mosquitto, mut sup, observer) = setup().await;
    let discovery_sub = observer
        .declare_subscriber("home/discovery/rf433")
        .await
        .expect("discovery subscriber");
    let mut mqtt = Mqtt::connect(mosquitto.port, "test-unbound").await;

    mqtt.publish(EVENTS, "8675309").await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let sample = tokio::time::timeout_at(deadline, discovery_sub.recv_async())
            .await
            .expect("a discovery record within 20s")
            .expect("discovery stream open");
        let records: serde_json::Value =
            serde_json::from_slice(&sample.payload().to_bytes()).expect("discovery is JSON");
        if let Some(found) = records
            .as_array()
            .and_then(|rs| rs.iter().find(|r| r["id"] == "8675309"))
        {
            assert_eq!(found["configured"], json!(false));
            assert_eq!(found["entity"], json!(null));
            break;
        }
    }

    sup.shutdown();
}

/// (e2) M26: an estate hears neighbours' remotes and RF noise all day —
/// unbound is unbounded traffic, not a fixed handful. Only the most
/// recently first-heard MAX_UNBOUND=200 stay in discovery; the oldest is
/// evicted, and the flood coalesces into the one publish this test reads
/// (the adapter debounces republish for DISCOVERY_COALESCE_S=5s).
#[tokio::test(flavor = "multi_thread")]
async fn unbound_discovery_stays_capped_at_two_hundred() {
    let (mosquitto, mut sup, observer) = setup().await;
    let discovery_sub = observer
        .declare_subscriber("home/discovery/rf433")
        .await
        .expect("discovery subscriber");
    let mut mqtt = Mqtt::connect(mosquitto.port, "test-cap").await;

    // 201 distinct unbound codes, first-heard in order 90000000..90000200.
    // One over the cap, so exactly the oldest (90000000) must be evicted.
    for code in 90_000_000..=90_000_200i64 {
        mqtt.publish(EVENTS, &code.to_string()).await;
    }

    // The debounce means only the FINAL state after the flood settles is
    // worth reading; poll until a record for the last code appears, which
    // can only happen once every code has been processed.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let records = loop {
        let sample = tokio::time::timeout_at(deadline, discovery_sub.recv_async())
            .await
            .expect("a discovery record within 20s")
            .expect("discovery stream open");
        let records: serde_json::Value =
            serde_json::from_slice(&sample.payload().to_bytes()).expect("discovery is JSON");
        let has_last = records
            .as_array()
            .is_some_and(|rs| rs.iter().any(|r| r["id"] == "90000200"));
        if has_last {
            break records;
        }
    };

    let unbound_ids: Vec<&str> = records
        .as_array()
        .expect("discovery is an array")
        .iter()
        .filter(|r| r["configured"] == json!(false))
        .filter_map(|r| r["id"].as_str())
        .collect();
    assert!(
        unbound_ids.len() <= 200,
        "unbound discovery grew past the cap: {} entries",
        unbound_ids.len()
    );
    assert!(
        !unbound_ids.contains(&"90000000"),
        "the oldest unbound code must be evicted once the cap is exceeded: {unbound_ids:?}"
    );
    assert!(
        unbound_ids.contains(&"90000200"),
        "the newest unbound code must be present: {unbound_ids:?}"
    );

    sup.shutdown();
}

/// (f) A bound entity's descriptor reaches discovery, and a detector's
/// field is notable — a smoke alarm firing has to land on Now as a
/// deviation, which is a descriptor fact rather than a state one.
#[tokio::test(flavor = "multi_thread")]
async fn descriptors_mark_a_detector_notable() {
    let (_mosquitto, mut sup, observer) = setup().await;
    // Published once at startup, so it is read from the mirror rather than
    // waited for on a subscriber.
    let records = loop {
        let replies = observer
            .get("home/discovery/rf433")
            .await
            .expect("discovery get");
        let mut found = None;
        while let Ok(reply) = replies.recv_async().await {
            if let Ok(sample) = reply.result() {
                if let Ok(value) =
                    serde_json::from_slice::<serde_json::Value>(&sample.payload().to_bytes())
                {
                    found = Some(value);
                }
            }
        }
        if let Some(value) = found {
            break value;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    let records = records.as_array().expect("discovery is a list").clone();

    let smoke = records
        .iter()
        .find(|r| r["id"] == SMOKE)
        .expect("the bound detector is listed before it has ever transmitted");
    assert_eq!(smoke["aspects"]["fields"]["smoke"]["notable"], json!(true));
    assert_eq!(
        smoke["aspects"]["fields"]["smoke"]["kind"],
        json!("boolean")
    );

    let door = records
        .iter()
        .find(|r| r["id"] == DOOR)
        .expect("the bound contact is listed");
    assert_eq!(door["aspects"]["fields"]["contact"]["notable"], json!(null));

    sup.shutdown();
}

/// (g) A payload carrying no usable code is reported rather than guessed
/// at.
#[tokio::test(flavor = "multi_thread")]
async fn unusable_payloads_are_reported() {
    let (mosquitto, mut sup, observer) = setup().await;
    let event_sub = observer
        .declare_subscriber(EVENT_KEY)
        .await
        .expect("event subscriber");
    let mut mqtt = Mqtt::connect(mosquitto.port, "test-malformed").await;

    mqtt.publish(EVENTS, "not-a-code").await;
    expect_drop_event(&event_sub, "malformed-payload").await;

    sup.shutdown();
}

/// (h) Availability is the bridge's LWT, not a receive timer: silence from
/// a one-way sender is its normal state and says nothing about the gateway.
#[tokio::test(flavor = "multi_thread")]
async fn availability_follows_the_bridge_lwt() {
    let (mosquitto, mut sup, observer) = setup().await;
    let state_sub = observer
        .declare_subscriber("home/state/**")
        .await
        .expect("state subscriber");
    let mut mqtt = Mqtt::connect(mosquitto.port, "test-lwt").await;

    mqtt.publish(LWT, "Online").await;
    expect_states(
        &state_sub,
        &[
            ("home/state/hallway/hallway_motion/available", json!(true)),
            ("home/state/landing/smoke_upstairs/available", json!(true)),
        ],
    )
    .await;

    mqtt.publish(LWT, "Offline").await;
    expect_states(
        &state_sub,
        &[("home/state/hallway/hallway_motion/available", json!(false))],
    )
    .await;

    sup.shutdown();
}
