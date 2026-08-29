//! ONVIF adapter integration tests: each scenario spawns a fake ONVIF
//! pull-point event service (tests/fake_onvif.py — real SOAP shapes, real
//! WS-Security digest checking, a genuine long poll) on a free port plus
//! the real supervisor on the onvif fixture house, and asserts on the bus.

mod common;

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use zenoh::sample::SampleKind;

use common::{await_mirror, expect_drop_event, expect_state, free_port, next_event, StateSub, Supervisor};

const FIXTURE: &str = "tests/fixture_house_onvif";
const CAMERAS_ENV: &str = "HOMEOSTAT_CAMERAS";
const EVENT_KEY: &str = "home/health/onvif/event";
const MOTION_KEY: &str = "home/state/hallway/hallway_cam/motion";
const AVAILABLE_KEY: &str = "home/state/hallway/hallway_cam/available";
const USERNAME: &str = "homeostat";
const PASSWORD: &str = "secret123";

/// A fake ONVIF camera (tests/fake_onvif.py) on a free port, killed on
/// drop. Spawned the same way the units themselves are: `uv run`.
struct FakeOnvif {
    child: Child,
    port: u16,
}

impl FakeOnvif {
    fn spawn() -> Self {
        let port = free_port();
        let child = Command::new("uv")
            .args([
                "run",
                "tests/fake_onvif.py",
                "--port",
                &port.to_string(),
                "--username",
                USERNAME,
                "--password",
                PASSWORD,
            ])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn fake onvif camera (is uv installed?)");
        // First run resolves the fake camera's own uv env: generous.
        let deadline = Instant::now() + Duration::from_secs(60);
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(Instant::now() < deadline, "fake onvif camera never listened on {port}");
            std::thread::sleep(Duration::from_millis(50));
        }
        Self { child, port }
    }

    /// POSTs a control endpoint (plain HTTP, out of the SOAP path).
    fn control(&self, path: &str) {
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", self.port))
            .expect("connect to fake camera control");
        stream
            .write_all(
                format!(
                    "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .expect("write control request");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("read control response");
        assert!(response.starts_with("HTTP/1.1 200"), "control {path}: {response}");
    }

    /// How many subscriptions the camera has handed out, from its own
    /// count — the only way to see a rotation from outside the adapter.
    fn created(&self) -> u64 {
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", self.port))
            .expect("connect to fake camera control");
        stream
            .write_all(
                "POST /control/stats HTTP/1.1\r\nHost: 127.0.0.1\r\n\
                 Content-Length: 0\r\nConnection: close\r\n\r\n"
                    .as_bytes(),
            )
            .expect("write stats request");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("read stats response");
        let body = response.rsplit("\r\n\r\n").next().expect("stats body");
        let value: Value = serde_json::from_str(body).expect("stats is JSON");
        value["created"].as_u64().expect("created is a number")
    }
}

impl Drop for FakeOnvif {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Writes a HOMEOSTAT_CAMERAS file (outside the repo, per the settlement)
/// giving the fixture's "hallway_cam" the fake camera's host and the
/// camera-account credentials.
fn cameras_file(port: u16) -> PathBuf {
    let path = std::env::temp_dir().join(format!("homeostat-cameras-{port}.toml"));
    std::fs::write(
        &path,
        format!(
            "[hallway_cam]\nhost = \"127.0.0.1:{port}\"\nusername = \"{USERNAME}\"\npassword = \"{PASSWORD}\"\n"
        ),
    )
    .expect("write cameras file");
    path
}

/// Spawns the fake camera + supervisor on the fixture and waits for the
/// adapter's liveliness token (generous timeout: first run resolves the
/// uv env for aiohttp too).
async fn setup() -> (FakeOnvif, PathBuf, Supervisor, zenoh::Session) {
    let camera = FakeOnvif::spawn();
    let cameras_path = cameras_file(camera.port);
    let sup = Supervisor::spawn_with_env(
        FIXTURE,
        &[(CAMERAS_ENV, cameras_path.to_str().expect("utf-8 path"))],
    );
    let observer = sup.observer().await;
    let token_sub = observer
        .liveliness()
        .declare_subscriber("home/health/onvif/alive")
        .history(true)
        .await
        .expect("liveliness subscriber");
    let token = tokio::time::timeout(Duration::from_secs(90), token_sub.recv_async())
        .await
        .expect("adapter liveliness token within 90s")
        .expect("liveliness stream open");
    assert_eq!(token.kind(), SampleKind::Put);
    // Ready fires with the subscription attempt merely in flight; a trigger
    // before the pull-point subscription exists lands in zero queues and is
    // lost (a real camera's events during an outage are too). available =
    // true is the adapter's own signal that the subscription is up.
    await_mirror(&observer, AVAILABLE_KEY, &json!(true)).await;
    (camera, cameras_path, sup, observer)
}

/// Triggers repeatedly until the motion key carries `expected`. Triggers
/// are lossy by design: one fans out only to the subscriptions existing
/// at that instant, and the adapter abandons its subscription on any
/// fault (a trigger stranded in an abandoned queue is a real camera's
/// event during an outage) — so every test drives triggers through a
/// retry, never one-shot.
async fn trigger_until_motion(camera: &FakeOnvif, sub: &StateSub, expected: bool) {
    let value = if expected { "true" } else { "false" };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        camera.control(&format!("/control/trigger?value={value}"));
        let recv = tokio::time::timeout(Duration::from_secs(1), sub.recv_async()).await;
        if let Ok(Ok(sample)) = recv {
            let value: Value = serde_json::from_slice(&sample.payload().to_bytes())
                .expect("state payload is JSON");
            if value == json!(expected) {
                return;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no motion = {expected} within 30s of re-triggering"
        );
    }
}

/// Like `trigger_until_motion`, for trigger values whose observable
/// effect is a health event rather than state.
async fn trigger_until_event(camera: &FakeOnvif, sub: &StateSub, value: &str, reason: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        camera.control(&format!("/control/trigger?value={value}"));
        let recv = tokio::time::timeout(Duration::from_secs(1), sub.recv_async()).await;
        if let Ok(Ok(sample)) = recv {
            let event: Value = serde_json::from_slice(&sample.payload().to_bytes())
                .expect("health event is JSON");
            if event["reason"] == reason {
                return;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no \"{reason}\" health event within 30s of re-triggering"
        );
    }
}

/// (a) On-camera motion events translate to the camera entity's `motion`
/// aspect — the event plane in one assertion: pixels stay off the bus,
/// detections ride it as ordinary scalar state.
#[tokio::test(flavor = "multi_thread")]
async fn motion_events_translate_to_bus_state() {
    let (camera, _cameras_path, mut sup, observer) = setup().await;
    let state_sub = observer.declare_subscriber(MOTION_KEY).await.expect("state subscriber");

    trigger_until_motion(&camera, &state_sub, true).await;
    trigger_until_motion(&camera, &state_sub, false).await;

    sup.shutdown();
}

/// (b) The Tapo-regression contract: a broken subscription (every pull
/// faults) emits one "event-stream-lost" health event and the adapter
/// resubscribes from scratch — events flow again without a restart.
#[tokio::test(flavor = "multi_thread")]
async fn broken_subscription_resubscribes() {
    let (camera, _cameras_path, mut sup, observer) = setup().await;
    let state_sub = observer.declare_subscriber(MOTION_KEY).await.expect("state subscriber");
    let event_sub = observer.declare_subscriber(EVENT_KEY).await.expect("event subscriber");

    trigger_until_motion(&camera, &state_sub, true).await;

    camera.control("/control/break");
    expect_drop_event(&event_sub, "event-stream-lost").await;
    trigger_until_motion(&camera, &state_sub, false).await;

    sup.shutdown();
}

/// (b2) Availability rides the subscription: losing it publishes
/// available = false alongside the health event, and the recreated
/// subscription publishes available = true again — while `motion` stands
/// untouched through the outage (stale, never false).
#[tokio::test(flavor = "multi_thread")]
async fn subscription_loss_flips_available() {
    let (camera, _cameras_path, mut sup, observer) = setup().await;
    let avail_sub = observer.declare_subscriber(AVAILABLE_KEY).await.expect("available subscriber");
    let state_sub = observer.declare_subscriber(MOTION_KEY).await.expect("state subscriber");

    trigger_until_motion(&camera, &state_sub, true).await;

    camera.control("/control/break");
    expect_state(&avail_sub, json!(false)).await;
    expect_state(&avail_sub, json!(true)).await;

    sup.shutdown();
}

/// (c) A notification that parses but carries an unusable value drops
/// with a "malformed-payload" health event — and the stream survives it.
#[tokio::test(flavor = "multi_thread")]
async fn malformed_motion_value_drops_with_health_event() {
    let (camera, _cameras_path, mut sup, observer) = setup().await;
    let state_sub = observer.declare_subscriber(MOTION_KEY).await.expect("state subscriber");
    let event_sub = observer.declare_subscriber(EVENT_KEY).await.expect("event subscriber");

    trigger_until_event(&camera, &event_sub, "banana", "malformed-payload").await;

    trigger_until_motion(&camera, &state_sub, true).await;

    sup.shutdown();
}

/// (d) The VP52 shape, diagnosed on real hardware: a Tapo answers
/// CreatePullPointSubscription and PullMessages with 200 and Renew with
/// 400, because it implements no WS-BaseNotification SubscriptionManager.
/// The pull stream is FINE, so this must not read as a stream loss — the
/// old behaviour tore the subscription down and flapped `available`
/// roughly every 17 s, indefinitely, against a live camera.
#[tokio::test(flavor = "multi_thread")]
async fn a_camera_without_a_subscription_manager_keeps_streaming() {
    let (camera, _cameras_path, mut sup, observer) = setup().await;
    let state_sub = observer.declare_subscriber(MOTION_KEY).await.expect("state subscriber");
    let avail_sub = observer.declare_subscriber(AVAILABLE_KEY).await.expect("available subscriber");
    let event_sub = observer.declare_subscriber(EVENT_KEY).await.expect("event subscriber");

    // available = true is published once, at the first subscription, which
    // is before this subscriber exists — so the assertion below is that
    // NOTHING arrives on it, i.e. no transition at all.
    trigger_until_motion(&camera, &state_sub, true).await;
    camera.control("/control/reject-renew");

    // The refusal is reported once, naming the call — and as its own kind,
    // not as a dropped message.
    let event = next_event(&event_sub).await;
    assert_eq!(event["kind"], json!("renew-unsupported"), "{event}");
    let error = event["error"].as_str().expect("error is a string");
    assert!(error.starts_with("Renew: HTTP 400"), "the call is named: {error}");

    // And the stream carries on: motion still flows, with no availability
    // transition at all. If the Renew fault were treated as a loss, an
    // available=false would arrive before this motion does.
    trigger_until_motion(&camera, &state_sub, false).await;
    trigger_until_motion(&camera, &state_sub, true).await;

    sup.shutdown();
    assert!(
        avail_sub.try_recv().expect("available channel open").is_none(),
        "availability must not flap: the pull stream was never lost"
    );
}

/// (e) ...and the subscription is ROTATED before it expires, which is what
/// keeps such a camera working past InitialTerminationTime. Without a
/// working Renew the stream would otherwise simply stop after PT60S.
/// Slow by nature: the rotation is a real wall-clock interval.
#[tokio::test(flavor = "multi_thread")]
async fn a_camera_without_a_subscription_manager_rotates_its_subscription() {
    let (camera, _cameras_path, mut sup, observer) = setup().await;
    let state_sub = observer.declare_subscriber(MOTION_KEY).await.expect("state subscriber");
    let avail_sub = observer.declare_subscriber(AVAILABLE_KEY).await.expect("available subscriber");

    trigger_until_motion(&camera, &state_sub, true).await;
    camera.control("/control/reject-renew");
    let before = camera.created();

    // RESUBSCRIBE_BEFORE_S is 40s against a PT60S termination.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    loop {
        if camera.created() > before {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the subscription was never rotated: still {before} after 90s"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    // The rotation is invisible from the bus: motion keeps flowing and
    // availability never moves, because nothing was ever lost.
    trigger_until_motion(&camera, &state_sub, false).await;
    assert!(
        avail_sub.try_recv().expect("available channel open").is_none(),
        "a rotation must not surface as an availability transition"
    );

    sup.shutdown();
}

/// (f) The other cause of a Renew fault, and the one CI caught as a race:
/// the subscription is genuinely GONE, and Renew is merely the call that
/// discovers it. Concluding "no SubscriptionManager" from the fault alone
/// would mark a perfectly capable camera as renew-less forever. The next
/// pull disambiguates — here it fails, so this is an ordinary loss.
#[tokio::test(flavor = "multi_thread")]
async fn a_renew_fault_from_a_lost_subscription_is_still_a_loss() {
    let (camera, _cameras_path, mut sup, observer) = setup().await;
    let state_sub = observer.declare_subscriber(MOTION_KEY).await.expect("state subscriber");
    let event_sub = observer.declare_subscriber(EVENT_KEY).await.expect("event subscriber");

    trigger_until_motion(&camera, &state_sub, true).await;
    camera.control("/control/break-on-renew");

    let event = next_event(&event_sub).await;
    assert_eq!(
        event["kind"], json!("drop"),
        "a vanished subscription is a loss, not a firmware quirk: {event}"
    );
    assert_eq!(event["reason"], json!("event-stream-lost"), "{event}");

    // ...and it recovers the ordinary way.
    trigger_until_motion(&camera, &state_sub, false).await;

    sup.shutdown();
}

/// (g) A notification is not a transition. A Tapo C200 re-asserts motion on
/// every evaluation tick — one real episode against VP52's cameras arrived
/// as 417 identical `true`s in 56 seconds, 456 recorded rows for two edges —
/// so `motion` publishes on CHANGE and the next sample on the key is always
/// the next edge.
#[tokio::test(flavor = "multi_thread")]
async fn repeated_notifications_publish_one_transition() {
    let (camera, _cameras_path, mut sup, observer) = setup().await;
    let state_sub = observer.declare_subscriber(MOTION_KEY).await.expect("state subscriber");

    trigger_until_motion(&camera, &state_sub, true).await;

    // The camera says what it has already said, repeatedly. These are not
    // lossy the way a first trigger is: the subscription that carried the
    // rising edge is still the live one.
    for _ in 0..10 {
        camera.control("/control/trigger?value=true");
    }
    let repeat = tokio::time::timeout(Duration::from_secs(5), state_sub.recv_async()).await;
    assert!(
        repeat.is_err(),
        "motion republished with no transition: {:?}",
        repeat.map(|s| s.map(|s| s.payload().try_to_string().map(|c| c.into_owned())))
    );

    // ...and a real edge still gets through. Asserting on the VALUE rather
    // than retrying until false is the point: a duplicate arriving here
    // must fail the test, not be waited past.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        camera.control("/control/trigger?value=false");
        let recv = tokio::time::timeout(Duration::from_secs(1), state_sub.recv_async()).await;
        if let Ok(Ok(sample)) = recv {
            let value: Value = serde_json::from_slice(&sample.payload().to_bytes())
                .expect("state payload is JSON");
            assert_eq!(value, json!(false), "the next sample after a rise must be the fall");
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "no falling edge within 30s");
    }

    sup.shutdown();
}
