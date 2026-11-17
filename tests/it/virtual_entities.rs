//! Virtual sensor integration test (docs/design.md#virtual-sensors). An
//! automation binds an entity and publishes derived state onto it. The
//! test shows the plan accepts automation-bound entities end to end, and
//! that the derived reading behaves like any other state: mirrored by the
//! core and published on transition only.
//!
//! Determinism: source puts go through publishers that have awaited a
//! matching subscriber (the evening.rs pattern), and the fused key is read
//! both live and through the core's state mirror. The mirror subscribes
//! `home/state/**` from supervisor start, so the writer does not filter
//! out the automation's put.

use std::time::Duration;

use serde_json::{json, Value};

use crate::common::{
    await_matching, await_mirror, config_write, matched_publisher, next_sample, restart_unit, up,
    StateSub, Supervisor,
};

const FIXTURE: &str = "tests/fixtures/house_virtual";
const FUSED_STATE: &str = "home/state/global/downstairs_temperature/temperature";
const LIVINGROOM_STATE: &str = "home/state/livingroom/thermo/temperature";
const OFFICE_STATE: &str = "home/state/office/thermo/temperature";

/// The next fused reading.
async fn next_fused(sub: &StateSub) -> Value {
    let msg = next_sample(sub, Duration::from_secs(10)).await;
    msg.expect("fused sample within 10s").value().clone()
}

/// The fused downstairs temperature: derived state lands on the virtual
/// entity's key, reaches the core mirror like any adapter's state, and is
/// republished only when the fusion moves.
#[tokio::test(flavor = "multi_thread")]
async fn virtual_sensor_publishes_fused_state() {
    let (mut sup, observer) = up(|| Supervisor::spawn(FIXTURE), &["fused_temperature"]).await;

    let fused_sub = observer
        .declare_subscriber(FUSED_STATE)
        .await
        .expect("fused subscriber");
    let livingroom = matched_publisher(&observer, LIVINGROOM_STATE).await;
    let office = matched_publisher(&observer, OFFICE_STATE).await;

    // One source: the fusion is that reading.
    livingroom.put(json!(20.0).to_string()).await.expect("put");
    assert_eq!(next_fused(&fused_sub).await, json!(20.0));
    await_mirror(&observer, FUSED_STATE, &json!(20.0)).await;

    // A second source at the same value does not move the mean, so nothing
    // publishes. Then a real move publishes once. One session's samples
    // arrive in order, so a next live sample of 21.0 shows no duplicate
    // 20.0 was published in between.
    office.put(json!(20.0).to_string()).await.expect("put");
    office.put(json!(22.0).to_string()).await.expect("put");
    assert_eq!(next_fused(&fused_sub).await, json!(21.0));
    await_mirror(&observer, FUSED_STATE, &json!(21.0)).await;

    sup.shutdown();
}

const MAX_AGE_KEY: &str = "home/config/fused_temperature/source_max_age_s";

/// A restarted automation is not blind until its sources publish again:
/// `ctx.subscribe` reads each source's current value from the core
/// mirror, so the fusion is published at once. The catch-up carries the
/// mirrored value's age, so a source older than the staleness policy is
/// left out of that first mean rather than passed off as fresh.
#[tokio::test(flavor = "multi_thread")]
async fn restarted_automation_catches_up_from_the_mirror() {
    let (mut sup, observer) = up(|| Supervisor::spawn(FIXTURE), &["fused_temperature"]).await;

    let fused_sub = observer
        .declare_subscriber(FUSED_STATE)
        .await
        .expect("fused subscriber");
    let livingroom = matched_publisher(&observer, LIVINGROOM_STATE).await;
    let office = matched_publisher(&observer, OFFICE_STATE).await;
    // One put at a time: the unit reads the two sources through two
    // subscribers, and zenoh orders samples within one, not across them.
    livingroom.put(json!(20.0).to_string()).await.expect("put");
    assert_eq!(next_fused(&fused_sub).await, json!(20.0));
    office.put(json!(22.0).to_string()).await.expect("put");
    assert_eq!(next_fused(&fused_sub).await, json!(21.0));
    await_mirror(&observer, LIVINGROOM_STATE, &json!(20.0)).await;
    await_mirror(&observer, OFFICE_STATE, &json!(22.0)).await;

    // The mirror serves each value with its age.
    let replies = observer.get(OFFICE_STATE).await.expect("mirror query");
    let reply = replies.recv_async().await.expect("mirror reply");
    let age: f64 = String::from_utf8(
        reply
            .result()
            .expect("ok reply")
            .attachment()
            .expect("mirror reply carries an age")
            .to_bytes()
            .to_vec(),
    )
    .expect("age is text")
    .parse()
    .expect("age is seconds");
    assert!((0.0..60.0).contains(&age), "age {age} s");

    // Restart with both sources within policy: the fusion is republished
    // from the catch-up alone, with no source publishing. Catch-up arrives one
    // key at a time and the unit recomputes on each, as on first start.
    restart_unit(&observer, "fused_temperature").await;
    assert_eq!(next_fused(&fused_sub).await, json!(20.0));
    assert_eq!(next_fused(&fused_sub).await, json!(21.0));

    // Tighten the policy so both mirrored values are stale by the next
    // restart. The catch-up then publishes nothing, and the first live
    // sample is fused alone: 30.0, not the 25.0 a fresh-looking catch-up
    // of livingroom = 20.0 would have produced.
    config_write(&observer, MAX_AGE_KEY, json!(1.0))
        .await
        .expect("policy write");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    restart_unit(&observer, "fused_temperature").await;
    await_matching(&office).await;
    office.put(json!(30.0).to_string()).await.expect("put");
    assert_eq!(next_fused(&fused_sub).await, json!(30.0));

    sup.shutdown();
}
