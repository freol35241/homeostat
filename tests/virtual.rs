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

use homeostat::bus::HealthStatus;
use serde_json::{json, Value};
use zenoh::handlers::FifoChannelHandler;
use zenoh::pubsub::Subscriber;
use zenoh::sample::Sample;

use common::{await_health, health_watch, Supervisor};

const FIXTURE: &str = "tests/fixture_house_virtual";
const FUSED_STATE: &str = "home/state/global/downstairs_temperature/temperature";
const LIVINGROOM_STATE: &str = "home/state/livingroom/thermo/temperature";
const OFFICE_STATE: &str = "home/state/office/thermo/temperature";

type Sub = Subscriber<FifoChannelHandler<Sample>>;
type Publisher = zenoh::pubsub::Publisher<'static>;

/// Declares a publisher and waits until a subscriber matches it, so
/// nothing this publisher puts is ever write-side filtered.
async fn matched_publisher(session: &zenoh::Session, key: &'static str) -> Publisher {
    let publisher = session.declare_publisher(key).await.expect("publisher");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let status = publisher.matching_status().await.expect("matching status");
        if status.matching() {
            return publisher;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no subscriber matched {}",
            publisher.key_expr()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

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
