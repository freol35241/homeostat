//! ONVIF adapter integration tests. Each scenario spawns a fake ONVIF
//! pull-point event service (`tests/fake_onvif.py`, with real SOAP shapes,
//! WS-Security digest checking and a long poll) on a free port plus the
//! real supervisor on the onvif fixture house, and asserts on the bus.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Value};

use crate::common::{
    assert_unit_contract, await_mirror, cache_read, expect_event_kind, expect_silence,
    expect_state, next_event, next_sample, up, FakeServer, Scratch, StateSub, Supervisor, SILENCE,
};

const FIXTURE: &str = "tests/fixtures/house_onvif";
const CAMERAS_ENV: &str = "HOMEOSTAT_CAMERAS";
const EVENT_KEY: &str = "home/health/onvif/event";
const MOTION_KEY: &str = "home/state/hallway/hallway_cam/motion";
const AVAILABLE_KEY: &str = "home/state/hallway/hallway_cam/available";
const USERNAME: &str = "homeostat";
const PASSWORD: &str = "secret123";

/// A fake ONVIF camera, with its count of handed-out subscriptions.
struct FakeOnvif(FakeServer);

impl FakeOnvif {
    async fn spawn() -> Self {
        Self(
            FakeServer::spawn(
                "tests/fake_onvif.py",
                &format!("--username {USERNAME} --password {PASSWORD}"),
            )
            .await,
        )
    }

    /// How many subscriptions the camera has handed out, from its own count.
    /// Outside the adapter, this is the only way to see a rotation.
    fn created(&self) -> u64 {
        let value: Value =
            serde_json::from_slice(&self.request("POST", "/control/stats")).expect("stats is JSON");
        value["created"].as_u64().expect("created is a number")
    }
}

impl std::ops::Deref for FakeOnvif {
    type Target = FakeServer;

    fn deref(&self) -> &FakeServer {
        &self.0
    }
}

/// Writes a `HOMEOSTAT_CAMERAS` file in `scratch` (outside the repo: it
/// holds credentials) giving the fixture's `hallway_cam` the fake camera's
/// host and the camera-account credentials.
fn cameras_file(scratch: &Scratch, port: u16) -> PathBuf {
    scratch.file(
        "cameras.toml",
        &format!(
            "[hallway_cam]\nhost = \"127.0.0.1:{port}\"\nusername = \"{USERNAME}\"\npassword = \"{PASSWORD}\"\n"
        ),
    )
}

/// Spawns the fake camera + supervisor on the fixture and waits for the
/// adapter to run.
async fn setup() -> (FakeOnvif, Scratch, Supervisor, zenoh::Session) {
    let camera = FakeOnvif::spawn().await;
    let cameras = Scratch::new("onvif-cameras");
    let cameras_path = cameras_file(&cameras, camera.port);
    let (sup, observer) = up(
        || {
            Supervisor::spawn_with_env(
                FIXTURE,
                &[(CAMERAS_ENV, cameras_path.to_str().expect("utf-8 path"))],
            )
        },
        &["onvif"],
    )
    .await;
    // Ready fires while the subscription attempt is still in flight; a trigger
    // before the pull-point subscription exists lands in zero queues and is
    // lost (a real camera's events during an outage are too). available =
    // true is the adapter's own signal that the subscription is up.
    await_mirror(&observer, AVAILABLE_KEY, &json!(true)).await;
    (camera, cameras, sup, observer)
}

/// Triggers repeatedly until the motion key carries `expected`. Triggers
/// are lossy: one fans out only to the subscriptions existing at that
/// instant, and the adapter abandons its subscription on any fault (a
/// trigger stranded in an abandoned queue is a real camera's event during
/// an outage). So every test drives triggers through a retry.
///
/// The retry alone is not enough, because `motion` publishes on change. A
/// subscription propagates to the publishing peer asynchronously after
/// `declare_subscriber().await` returns locally, so a sample published
/// inside that window is lost. Re-triggering the same value cannot
/// produce another, because the adapter has already published it. So each
/// miss also asks the core's last-value mirror (`home/state/**`), which is
/// request/response and not subject to the race: it answers whether the
/// edge happened at all.
async fn trigger_until_motion(
    camera: &FakeOnvif,
    sub: &StateSub,
    session: &zenoh::Session,
    expected: bool,
) {
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
        } else if cache_read(session, MOTION_KEY).await == Some(json!(expected)) {
            return;
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
            let event: Value =
                serde_json::from_slice(&sample.payload().to_bytes()).expect("health event is JSON");
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
/// aspect. This is the event plane in one assertion: pixels stay off the
/// bus, and detections ride it as ordinary scalar state.
#[tokio::test(flavor = "multi_thread")]
async fn motion_events_translate_to_bus_state() {
    let (camera, _cameras, mut sup, observer) = setup().await;
    let state_sub = observer
        .declare_subscriber(MOTION_KEY)
        .await
        .expect("state subscriber");

    trigger_until_motion(&camera, &state_sub, &observer, true).await;
    trigger_until_motion(&camera, &state_sub, &observer, false).await;

    sup.shutdown();
}

/// (b) The broken-subscription contract: a broken subscription (every pull
/// faults) emits one "event-stream-lost" health event and the adapter
/// resubscribes from scratch, so events flow again without a restart.
#[tokio::test(flavor = "multi_thread")]
async fn broken_subscription_resubscribes() {
    let (camera, _cameras, mut sup, observer) = setup().await;
    let state_sub = observer
        .declare_subscriber(MOTION_KEY)
        .await
        .expect("state subscriber");
    let event_sub = observer
        .declare_subscriber(EVENT_KEY)
        .await
        .expect("event subscriber");

    trigger_until_motion(&camera, &state_sub, &observer, true).await;

    camera.control("/control/break");
    expect_event_kind(&event_sub, "event-stream-lost").await;
    trigger_until_motion(&camera, &state_sub, &observer, false).await;

    sup.shutdown();
}

/// (b2) Availability rides the subscription: losing it publishes
/// available = false alongside the health event, and the recreated
/// subscription publishes available = true again. `motion` stays
/// untouched through the outage: stale, but not false.
#[tokio::test(flavor = "multi_thread")]
async fn subscription_loss_flips_available() {
    let (camera, _cameras, mut sup, observer) = setup().await;
    let avail_sub = observer
        .declare_subscriber(AVAILABLE_KEY)
        .await
        .expect("available subscriber");
    let state_sub = observer
        .declare_subscriber(MOTION_KEY)
        .await
        .expect("state subscriber");

    trigger_until_motion(&camera, &state_sub, &observer, true).await;

    camera.control("/control/break");
    expect_state(&avail_sub, json!(false)).await;
    expect_state(&avail_sub, json!(true)).await;

    sup.shutdown();
}

/// (c) A notification that parses but carries an unusable value drops
/// with a "malformed-payload" health event, and the stream survives it.
#[tokio::test(flavor = "multi_thread")]
async fn malformed_motion_value_drops_with_health_event() {
    let (camera, _cameras, mut sup, observer) = setup().await;
    let state_sub = observer
        .declare_subscriber(MOTION_KEY)
        .await
        .expect("state subscriber");
    let event_sub = observer
        .declare_subscriber(EVENT_KEY)
        .await
        .expect("event subscriber");

    trigger_until_event(&camera, &event_sub, "banana", "malformed-payload").await;

    trigger_until_motion(&camera, &state_sub, &observer, true).await;

    sup.shutdown();
}

/// (d) A real Tapo camera answers `CreatePullPointSubscription` and
/// `PullMessages` with 200 and Renew with 400, because it implements no
/// WS-BaseNotification `SubscriptionManager`. The pull stream is fine, so
/// this must not read as a stream loss. Treating it as one tears the
/// subscription down and flaps `available` roughly every 17 s against a
/// live camera.
#[tokio::test(flavor = "multi_thread")]
async fn a_camera_without_a_subscription_manager_keeps_streaming() {
    let (camera, _cameras, mut sup, observer) = setup().await;
    let state_sub = observer
        .declare_subscriber(MOTION_KEY)
        .await
        .expect("state subscriber");
    let avail_sub = observer
        .declare_subscriber(AVAILABLE_KEY)
        .await
        .expect("available subscriber");
    let event_sub = observer
        .declare_subscriber(EVENT_KEY)
        .await
        .expect("event subscriber");

    // available = true is published once, at the first subscription, which
    // is before this subscriber exists. So the assertion below is that
    // nothing arrives on it: no transition at all.
    trigger_until_motion(&camera, &state_sub, &observer, true).await;
    camera.control("/control/reject-renew");

    // The refusal is reported once, naming the call, under its own kind
    // rather than as a dropped message.
    let event = next_event(&event_sub).await;
    assert_eq!(event["kind"], json!("renew-unsupported"), "{event}");
    let error = event["error"].as_str().expect("error is a string");
    assert!(
        error.starts_with("Renew: HTTP 400"),
        "the call is named: {error}"
    );

    // And the stream carries on: motion still flows, with no availability
    // transition at all. If the Renew fault were treated as a loss, an
    // available=false would arrive before this motion does.
    trigger_until_motion(&camera, &state_sub, &observer, false).await;
    trigger_until_motion(&camera, &state_sub, &observer, true).await;

    sup.shutdown();
    assert!(
        avail_sub
            .try_recv()
            .expect("available channel open")
            .is_none(),
        "availability must not flap: the pull stream was never lost"
    );
}

/// (e) ...and the subscription is rotated before it expires, which keeps
/// such a camera working past `InitialTerminationTime`. Without a working
/// Renew the stream would stop after PT60S. The rotation is a real
/// wall-clock interval, which the fixture shortens through
/// `resubscribe_after_s`.
#[tokio::test(flavor = "multi_thread")]
async fn a_camera_without_a_subscription_manager_rotates_its_subscription() {
    let (camera, _cameras, mut sup, observer) = setup().await;
    let state_sub = observer
        .declare_subscriber(MOTION_KEY)
        .await
        .expect("state subscriber");
    let avail_sub = observer
        .declare_subscriber(AVAILABLE_KEY)
        .await
        .expect("available subscriber");

    trigger_until_motion(&camera, &state_sub, &observer, true).await;
    camera.control("/control/reject-renew");
    let before = camera.created();

    // The fixture sets resubscribe_after_s to 3, so the rotation comes
    // within one pull of that.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if camera.created() > before {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the subscription was never rotated: still {before} after 30s"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // The rotation is invisible from the bus: motion keeps flowing and
    // availability does not move, because nothing was lost.
    trigger_until_motion(&camera, &state_sub, &observer, false).await;
    assert!(
        avail_sub
            .try_recv()
            .expect("available channel open")
            .is_none(),
        "a rotation must not surface as an availability transition"
    );

    sup.shutdown();
}

/// (f) The other cause of a Renew fault, which races with the first:
/// the subscription is gone, and Renew is the call that discovers it.
/// Concluding "no `SubscriptionManager`" from the fault alone would mark a
/// capable camera as renew-less for good. The next pull tells the two
/// apart. Here it fails, so this is an ordinary loss.
#[tokio::test(flavor = "multi_thread")]
async fn a_renew_fault_from_a_lost_subscription_is_still_a_loss() {
    let (camera, _cameras, mut sup, observer) = setup().await;
    let state_sub = observer
        .declare_subscriber(MOTION_KEY)
        .await
        .expect("state subscriber");
    let event_sub = observer
        .declare_subscriber(EVENT_KEY)
        .await
        .expect("event subscriber");

    trigger_until_motion(&camera, &state_sub, &observer, true).await;
    camera.control("/control/break-on-renew");

    let event = next_event(&event_sub).await;
    assert_eq!(
        event["kind"],
        json!("event-stream-lost"),
        "a vanished subscription is a loss, not a firmware quirk: {event}"
    );

    // ...and it recovers the ordinary way.
    trigger_until_motion(&camera, &state_sub, &observer, false).await;

    sup.shutdown();
}

/// (g) Repeated notifications of one value publish once. A Tapo C200
/// re-asserts motion on every evaluation tick: one real episode sent 417
/// identical `true`s in 56 seconds for two edges. So `motion` publishes on
/// change, and the next sample on the key is the next edge.
#[tokio::test(flavor = "multi_thread")]
async fn repeated_notifications_publish_one_transition() {
    let (camera, _cameras, mut sup, observer) = setup().await;
    let state_sub = observer
        .declare_subscriber(MOTION_KEY)
        .await
        .expect("state subscriber");

    trigger_until_motion(&camera, &state_sub, &observer, true).await;

    // The camera says what it has already said, repeatedly. These are not
    // lossy the way a first trigger is: the subscription that carried the
    // rising edge is still the live one.
    for _ in 0..10 {
        camera.control("/control/trigger?value=true");
    }
    expect_silence(&state_sub, SILENCE, "motion republished with no transition").await;

    // ...and a real edge still gets through. The test asserts on the next
    // sample's value instead of retrying until false, so a duplicate
    // arriving here fails the test.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        camera.control("/control/trigger?value=false");
        if let Some(sample) = next_sample(&state_sub, Duration::from_secs(1)).await {
            assert_eq!(
                *sample.value(),
                json!(false),
                "the next sample after a rise must be the fall"
            );
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no falling edge within 30s"
        );
    }

    sup.shutdown();
}

/// (i) A camera that streams its SOAP replies is read whole. Real ONVIF
/// firmware answers with `Transfer-Encoding: chunked`, and aiohttp's
/// `content.read(n)` hands back only what is buffered (the first chunk),
/// so a single read truncates the envelope mid-document and every reply
/// fails to parse ("unparseable response: unclosed token: line 2, column
/// 0"). Every other test here passes against a body that arrives in one
/// piece, so a one-shot read would pass them all. This one catches it.
///
/// The chunking is switched on mid-run, after motion has already been
/// proved to work, so the assertion is about the framing and nothing else.
#[tokio::test(flavor = "multi_thread")]
async fn a_chunked_soap_reply_is_read_whole() {
    let (camera, _cameras, mut sup, observer) = setup().await;
    let state_sub = observer
        .declare_subscriber(MOTION_KEY)
        .await
        .expect("state subscriber");

    // The control: unchunked, motion arrives.
    trigger_until_motion(&camera, &state_sub, &observer, true).await;

    camera.control("/control/chunked");
    trigger_until_motion(&camera, &state_sub, &observer, false).await;

    sup.shutdown();
}

/// The adapter honors the unit contract: liveliness token when ready,
/// clean SIGTERM shutdown within the grace, no orphans.
#[tokio::test(flavor = "multi_thread")]
async fn adapter_honors_unit_contract() {
    let (_camera, _cameras, mut sup, observer) = setup().await;
    assert_unit_contract(&mut sup, &observer, "onvif").await;
}
