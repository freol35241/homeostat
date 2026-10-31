//! Persons integration test (docs/design.md#map-and-people). The shipped
//! `persons` automation binds a person and computes it from the trackers
//! its `[sources]` list. The reflector adapter binds the trackers, and the
//! test publishes their readings as the adapters would.
//!
//! The rule itself is unit-tested in `tests/adapters/test_persons.py`. This
//! test shows it running against real processes: the plan accepts a person
//! built from trackers, the unit reads them off the bus, and the person's
//! aspects and its decisions land where the dashboard reads them.

mod common;

use std::time::Duration;

use homeostat::bus::HealthStatus;
use serde_json::{json, Value};

use common::{await_health, expect_state, health_watch, matched_publisher, Publisher, Supervisor};

const FIXTURE: &str = "tests/fixtures/house_persons";
const PERSON: &str = "home/state/person/fredrik";
const PHONE: &str = "home/state/person/fredrik_phone";
const ROUTER_SIGHTING: &str = "home/state/person/fredriks_phone/sighted";
const EVENT_KEY: &str = "home/health/persons/event";

async fn put(publisher: &Publisher, value: Value) {
    publisher.put(value.to_string()).await.expect("state put");
}

/// Waits for a `decision` event on the person's rule whose outcome starts
/// with `prefix`.
async fn expect_decision(sub: &common::StateSub, prefix: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let sample = tokio::time::timeout_at(deadline, sub.recv_async())
            .await
            .unwrap_or_else(|_| panic!("no decision \"{prefix}…\" within 20s"))
            .expect("event stream open");
        let event: Value =
            serde_json::from_slice(&sample.payload().to_bytes()).expect("health event is JSON");
        if event["kind"] == "decision"
            && event["rule"] == "fredrik"
            && event["outcome"]
                .as_str()
                .is_some_and(|o| o.starts_with(prefix))
        {
            return;
        }
    }
}

/// A geofence and a router sighting make one person: home on either, away
/// only when the geofence says he left, stale when nothing can say, and
/// positioned from the phone's fix.
#[tokio::test(flavor = "multi_thread")]
async fn trackers_make_one_person() {
    let mut sup = Supervisor::spawn(FIXTURE);
    let observer = sup.observer().await;
    let mut persons = health_watch(&observer, "persons").await;
    await_health(&mut persons, Duration::from_mins(2), |h| {
        h.status == HealthStatus::Running
    })
    .await;

    let presence = observer
        .declare_subscriber(format!("{PERSON}/presence"))
        .await
        .expect("presence subscriber");
    let position = observer
        .declare_subscriber(format!("{PERSON}/*"))
        .await
        .expect("person subscriber");
    let events = observer
        .declare_subscriber(EVENT_KEY)
        .await
        .expect("event subscriber");
    let geofence = matched_publisher(&observer, &format!("{PHONE}/home")).await;
    let wifi = matched_publisher(&observer, ROUTER_SIGHTING).await;

    // The router sees him first: home, before the geofence says anything.
    put(&wifi, json!(true)).await;
    expect_state(&presence, json!(true)).await;
    expect_decision(&events, "home: wifi").await;

    // The phone sleeps: a lost sighting is no evidence he left.
    put(&geofence, json!(true)).await;
    put(&wifi, json!(false)).await;
    expect_decision(&events, "home: geofence").await;

    // He leaves the geofence: away.
    put(&geofence, json!(false)).await;
    expect_state(&presence, json!(false)).await;
    expect_decision(&events, "away: geofence").await;

    // The phone drops off the broker. Its last geofence no longer counts,
    // the router cannot say he left, so presence is held and marked stale.
    let phone_available = matched_publisher(&observer, &format!("{PHONE}/available")).await;
    put(&phone_available, json!(false)).await;
    expect_decision(&events, "unchanged").await;
    common::expect_states(
        &position,
        &[
            ("home/state/person/fredrik/presence", json!(false)),
            ("home/state/person/fredrik/presence_valid", json!(false)),
        ],
    )
    .await;

    // A fix from the phone becomes the person's position.
    for (aspect, value) in [
        ("lat", json!(57.7)),
        ("lon", json!(11.97)),
        ("accuracy", json!(12)),
        ("fixed_at", json!(1_800_000_000)),
    ] {
        let publisher = matched_publisher(&observer, &format!("{PHONE}/{aspect}")).await;
        put(&publisher, value).await;
    }
    common::expect_states(
        &position,
        &[
            ("home/state/person/fredrik/lat", json!(57.7)),
            ("home/state/person/fredrik/lon", json!(11.97)),
            ("home/state/person/fredrik/accuracy", json!(12)),
            ("home/state/person/fredrik/fixed_at", json!(1_800_000_000)),
        ],
    )
    .await;

    sup.shutdown();
}
