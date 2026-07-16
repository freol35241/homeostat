//! Arbiter service integration tests: a real supervisor on the arbiter
//! fixture house (no broker — the arbiter is a pure bus service), asserting
//! on the forward/preempt/refuse/expiry contract of docs/design.md,
//! Arbitrated mode ("Settled 2026-07-16").

mod common;

use std::time::Duration;

use homeostat::bus::HealthStatus;
use serde_json::{json, Value};
use zenoh::handlers::FifoChannelHandler;
use zenoh::pubsub::Subscriber;

use common::{await_health, health_watch, process_alive, Supervisor};

const FIXTURE: &str = "tests/fixture_house_arbiter";
const CMD_KEY: &str = "home/cmd/hallway/front_door/locked";
const ARBITER_KEY: &str = "home/arbiter/hallway/front_door/locked";
const EVENT_KEY: &str = "home/health/arbiter/event";
const HOLD_MINUTES_KEY: &str = "home/config/arbiter/hold_minutes";

type Sub = Subscriber<FifoChannelHandler<zenoh::sample::Sample>>;

/// Spawns the fixture and waits for the arbiter's liveliness token.
async fn setup() -> (Supervisor, zenoh::Session) {
    let sup = Supervisor::spawn(FIXTURE);
    let observer = sup.observer().await;
    let mut watch = health_watch(&observer, "arbiter").await;
    await_health(&mut watch, Duration::from_secs(60), |h| {
        h.status == HealthStatus::Running
    })
    .await;
    (sup, observer)
}

fn envelope(value: Value, priority: &str, actor: &str) -> Value {
    json!({"value": value, "priority": priority, "actor": actor})
}

async fn put_cmd(session: &zenoh::Session, payload: &Value) {
    session
        .put(CMD_KEY, payload.to_string())
        .await
        .expect("cmd put");
}

/// Writes a parameter through the core's query-with-payload write path
/// (same mechanic as tests/evening.rs's config_write and the dashboard's
/// /api/param).
async fn config_write(session: &zenoh::Session, key: &str, value: Value) -> Value {
    let replies = session
        .get(key)
        .payload(value.to_string())
        .await
        .expect("config write query");
    let reply = replies.recv_async().await.expect("config write reply");
    serde_json::from_slice(&reply.result().expect("write accepted").payload().to_bytes())
        .expect("ok reply is JSON")
}

/// Next sample within the timeout, decoded as JSON.
async fn next_json(sub: &Sub, timeout: Duration) -> Option<Value> {
    let sample = tokio::time::timeout(timeout, sub.recv_async()).await.ok()?.expect("stream open");
    Some(serde_json::from_slice(&sample.payload().to_bytes()).expect("payload is JSON"))
}

/// Asserts no message arrives within the window.
async fn expect_silence(sub: &Sub, window: Duration, what: &str) {
    if let Ok(Ok(sample)) = tokio::time::timeout(window, sub.recv_async()).await {
        panic!(
            "unexpected {what}: {}",
            String::from_utf8_lossy(&sample.payload().to_bytes())
        );
    }
}

/// (a)(b)(c)(d) The full lease lifecycle on one arbitrated entity: an
/// automation wish forwards and takes the lease; a manual wish preempts it
/// (event fired) and forwards; a subsequent automation wish is refused (no
/// forward, event fired); shrinking hold_minutes to a sub-second value and
/// waiting it out reopens the entity, so automation wishes flow again.
#[tokio::test(flavor = "multi_thread")]
async fn forward_preempt_refuse_and_expiry() {
    let (mut sup, observer) = setup().await;
    let arbiter_sub = observer
        .declare_subscriber(ARBITER_KEY)
        .await
        .expect("arbiter subscriber");
    let event_sub = observer
        .declare_subscriber(EVENT_KEY)
        .await
        .expect("event subscriber");

    // (a) No holder yet: an automation wish forwards unchanged and takes
    // the lease at the automation band.
    let auto_wish = envelope(json!(true), "automation", "scheduler");
    put_cmd(&observer, &auto_wish).await;
    let forwarded = next_json(&arbiter_sub, Duration::from_secs(10))
        .await
        .expect("automation wish forwarded");
    assert_eq!(forwarded, auto_wish, "envelope forwarded unchanged");
    expect_silence(&event_sub, Duration::from_millis(500), "event on a clean take").await;

    // Shrink hold_minutes now, before the manual takeover below: the next
    // lease taken (by the manual wish) is computed from the new value, so
    // it will expire in well under a second once we wait it out in (d).
    let written = config_write(&observer, HOLD_MINUTES_KEY, json!(0.01)).await;
    assert_eq!(written, json!(0.01));

    // (b) A manual wish preempts the still-active (strictly lower)
    // automation holder: preempt event, then the forward.
    let manual_wish = envelope(json!(false), "manual", "owner");
    put_cmd(&observer, &manual_wish).await;
    let event = next_json(&event_sub, Duration::from_secs(10))
        .await
        .expect("preempt event");
    assert_eq!(
        event,
        json!({
            "kind": "preempt",
            "room": "hallway",
            "entity": "front_door",
            "aspect": "locked",
            "from_priority": "automation",
            "from_actor": "scheduler",
            "to_priority": "manual",
            "to_actor": "owner",
        })
    );
    let forwarded = next_json(&arbiter_sub, Duration::from_secs(10))
        .await
        .expect("manual wish forwarded");
    assert_eq!(forwarded, manual_wish, "envelope forwarded unchanged");

    // (c) An automation wish right afterwards is refused: the manual lease
    // (just taken, hold_minutes now 0.01 = 0.6s) is still active and
    // strictly higher. No forward; a refuse event instead.
    put_cmd(&observer, &auto_wish).await;
    let event = next_json(&event_sub, Duration::from_secs(10))
        .await
        .expect("refuse event");
    assert_eq!(
        event,
        json!({
            "kind": "refuse",
            "room": "hallway",
            "entity": "front_door",
            "aspect": "locked",
            "priority": "automation",
            "actor": "scheduler",
            "holder_priority": "manual",
            "holder_actor": "owner",
        })
    );
    expect_silence(&arbiter_sub, Duration::from_millis(500), "forward of a refused wish").await;

    // (d) Once the shrunk lease's deadline passes, the entity reopens: the
    // same automation wish now forwards again, taking a fresh lease.
    tokio::time::sleep(Duration::from_millis(900)).await;
    put_cmd(&observer, &auto_wish).await;
    let forwarded = next_json(&arbiter_sub, Duration::from_secs(10))
        .await
        .expect("automation wish forwards again after expiry");
    assert_eq!(forwarded, auto_wish);
    expect_silence(&event_sub, Duration::from_millis(500), "event on an expiry re-take").await;

    sup.shutdown();
}

/// (f) A malformed cmd envelope for an arbitrated entity drops with an
/// "invalid-command" health event, and never reaches home/arbiter/**.
#[tokio::test(flavor = "multi_thread")]
async fn malformed_envelope_drops_with_health_event() {
    let (mut sup, observer) = setup().await;
    let arbiter_sub = observer
        .declare_subscriber(ARBITER_KEY)
        .await
        .expect("arbiter subscriber");
    let event_sub = observer
        .declare_subscriber(EVENT_KEY)
        .await
        .expect("event subscriber");

    // A bare value (the pre-envelope shape) is not an envelope.
    observer.put(CMD_KEY, "true").await.expect("cmd put");
    let event = next_json(&event_sub, Duration::from_secs(10))
        .await
        .expect("invalid-command event");
    assert_eq!(event["kind"], json!("drop"));
    assert_eq!(event["reason"], json!("invalid-command"));
    expect_silence(&arbiter_sub, Duration::from_millis(1500), "forward of a malformed command")
        .await;

    // An envelope with an unknown priority is just as malformed.
    observer
        .put(CMD_KEY, json!({"value": true, "priority": "urgent", "actor": "x"}).to_string())
        .await
        .expect("cmd put");
    let event = next_json(&event_sub, Duration::from_secs(10))
        .await
        .expect("invalid-command event");
    assert_eq!(event["reason"], json!("invalid-command"));

    // Still alive and translating afterwards.
    let ok_wish = envelope(json!(true), "automation", "scheduler");
    put_cmd(&observer, &ok_wish).await;
    let forwarded = next_json(&arbiter_sub, Duration::from_secs(10))
        .await
        .expect("a well-formed wish still forwards");
    assert_eq!(forwarded, ok_wish);

    sup.shutdown();
}

/// (e) The arbiter honors the step-2 unit contract: liveliness token when
/// ready, clean SIGTERM shutdown within the grace, no orphans.
#[tokio::test(flavor = "multi_thread")]
async fn adapter_honors_unit_contract() {
    let (mut sup, observer) = setup().await;
    let mut watch = health_watch(&observer, "arbiter").await;
    let health = await_health(&mut watch, Duration::from_secs(10), |h| {
        h.status == HealthStatus::Running
    })
    .await;
    let pid = health.pid.expect("running unit has a pid");
    assert!(process_alive(pid), "arbiter alive before shutdown");

    sup.signal(libc::SIGTERM);
    // shutdown_grace_s = 5 in the fixture; a graceful exit must fit inside
    // it with margin only for reaping and bus teardown.
    let code = sup.wait_exit(Duration::from_secs(7));
    assert_eq!(code, Some(0), "supervisor exit code");
    assert!(!process_alive(pid), "arbiter must not outlive the supervisor");
}
