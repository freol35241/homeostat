//! Virtual sensor integration test (docs/design.md, Virtual sensors): an
//! automation binds an entity and publishes derived state onto it, proving
//! the plan accepts automation-bound entities end to end and the derived
//! reading behaves like any other state — mirrored by the core, published
//! on transition only.
//!
//! Determinism: source puts go through publishers that have awaited a
//! matching subscriber (the evening.rs pattern), and the fused key is read
//! both live and through the core's state mirror — the mirror subscribes
//! `home/state/**` from supervisor start, so the automation's put is never
//! writer-side filtered.

mod common;

use std::time::Duration;

use homeostat::bus::{Health, HealthStatus};
use serde_json::{json, Value};
use zenoh::handlers::FifoChannelHandler;
use zenoh::pubsub::Subscriber;
use zenoh::sample::Sample;

use common::{
    await_health, await_matching, config_write, health_watch, matched_publisher, Supervisor,
};

const FIXTURE: &str = "tests/fixture_house_virtual";
const FUSED_STATE: &str = "home/state/global/downstairs_temperature/temperature";
const LIVINGROOM_STATE: &str = "home/state/livingroom/thermo/temperature";
const OFFICE_STATE: &str = "home/state/office/thermo/temperature";

type Sub = Subscriber<FifoChannelHandler<Sample>>;

async fn next_sample(sub: &Sub, timeout: Duration) -> Value {
    let sample = tokio::time::timeout(timeout, sub.recv_async())
        .await
        .expect("fused sample within timeout")
        .expect("state stream open");
    serde_json::from_slice(&sample.payload().to_bytes()).expect("state payload is JSON")
}

/// Reads a concrete key from the core's last-value state mirror, retrying
/// until it holds the expected value.
async fn mirror_read_eventually(session: &zenoh::Session, key: &str, expected: &Value) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let replies = session.get(key).await.expect("mirror query");
        while let Ok(reply) = replies.recv_async().await {
            if let Ok(sample) = reply.result() {
                let value: Value = serde_json::from_slice(&sample.payload().to_bytes())
                    .expect("mirror reply is JSON");
                if &value == expected {
                    return;
                }
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "mirror never held {expected} at {key}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The fused downstairs temperature: derived state lands on the virtual
/// entity's key, reaches the core mirror like any adapter's state, and is
/// republished only when the fusion actually moves.
#[tokio::test(flavor = "multi_thread")]
async fn virtual_sensor_publishes_fused_state() {
    let mut sup = Supervisor::spawn(FIXTURE);
    let observer = sup.observer().await;

    let mut automation = health_watch(&observer, "fused_temperature").await;
    await_health(&mut automation, Duration::from_secs(120), |h| {
        h.status == HealthStatus::Running
    })
    .await;

    let fused_sub = observer
        .declare_subscriber(FUSED_STATE)
        .await
        .expect("fused subscriber");
    let livingroom = matched_publisher(&observer, LIVINGROOM_STATE).await;
    let office = matched_publisher(&observer, OFFICE_STATE).await;

    // One source: the fusion is that reading.
    livingroom.put(json!(20.0).to_string()).await.expect("put");
    assert_eq!(next_sample(&fused_sub, Duration::from_secs(10)).await, json!(20.0));
    mirror_read_eventually(&observer, FUSED_STATE, &json!(20.0)).await;

    // A second source at the same value does not move the mean: publish on
    // transition only. Then a real move publishes exactly once — the next
    // live sample being 21.0 (same-session FIFO) proves no duplicate 20.0
    // was ever published in between.
    office.put(json!(20.0).to_string()).await.expect("put");
    office.put(json!(22.0).to_string()).await.expect("put");
    assert_eq!(next_sample(&fused_sub, Duration::from_secs(10)).await, json!(21.0));
    mirror_read_eventually(&observer, FUSED_STATE, &json!(21.0)).await;

    sup.shutdown();
}

const MAX_AGE_KEY: &str = "home/config/fused_temperature/source_max_age_s";

/// Kills the automation's process and waits for the supervisor's fresh
/// incarnation to report running under a new pid.
async fn restart_automation(observer: &zenoh::Session, pid: u32) -> Health {
    let mut watch = health_watch(observer, "fused_temperature").await;
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
    await_health(&mut watch, Duration::from_secs(60), |h| {
        h.status == HealthStatus::Running && h.pid != Some(pid)
    })
    .await
}

/// A restarted automation is not blind until its sources publish again
/// (#36): `ctx.subscribe` reads each source's current value from the core
/// mirror, so the fusion is published at once. The catch-up carries the
/// mirrored value's age, so a source older than the staleness policy is
/// left out of that first mean rather than passed off as fresh.
#[tokio::test(flavor = "multi_thread")]
async fn restarted_automation_catches_up_from_the_mirror() {
    let mut sup = Supervisor::spawn(FIXTURE);
    let observer = sup.observer().await;

    let mut automation = health_watch(&observer, "fused_temperature").await;
    let running = await_health(&mut automation, Duration::from_secs(120), |h| {
        h.status == HealthStatus::Running
    })
    .await;

    let fused_sub = observer
        .declare_subscriber(FUSED_STATE)
        .await
        .expect("fused subscriber");
    let livingroom = matched_publisher(&observer, LIVINGROOM_STATE).await;
    let office = matched_publisher(&observer, OFFICE_STATE).await;
    // One put at a time: the unit reads the two sources through two
    // subscribers, and zenoh orders samples within one, not across them.
    livingroom.put(json!(20.0).to_string()).await.expect("put");
    assert_eq!(next_sample(&fused_sub, Duration::from_secs(10)).await, json!(20.0));
    office.put(json!(22.0).to_string()).await.expect("put");
    assert_eq!(next_sample(&fused_sub, Duration::from_secs(10)).await, json!(21.0));
    mirror_read_eventually(&observer, LIVINGROOM_STATE, &json!(20.0)).await;
    mirror_read_eventually(&observer, OFFICE_STATE, &json!(22.0)).await;

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
    // from the catch-up alone — no source publishes. Catch-up arrives one
    // key at a time and the unit recomputes on each, as on first start.
    let restarted = restart_automation(&observer, running.pid.expect("pid")).await;
    assert_eq!(next_sample(&fused_sub, Duration::from_secs(10)).await, json!(20.0));
    assert_eq!(next_sample(&fused_sub, Duration::from_secs(10)).await, json!(21.0));

    // Tighten the policy so both mirrored values are stale by the next
    // restart. The catch-up then publishes nothing, and the first live
    // sample is fused alone: 30.0, not the 25.0 a fresh-looking catch-up
    // of livingroom = 20.0 would have produced.
    config_write(&observer, MAX_AGE_KEY, json!(1.0))
        .await
        .expect("policy write");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    restart_automation(&observer, restarted.pid.expect("pid")).await;
    await_matching(&office).await;
    office.put(json!(30.0).to_string()).await.expect("put");
    assert_eq!(next_sample(&fused_sub, Duration::from_secs(10)).await, json!(30.0));

    sup.shutdown();
}
