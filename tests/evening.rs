//! Step-4 integration tests: the clock service, the evening_lights
//! automation, and the live parameter path, each scenario on a real
//! supervisor.
//!
//! The off-time-crossing scenarios run on the clock-less sim fixture: the
//! test process publishes `home/clock/minute` itself, so no scenario ever
//! waits out a wall-clock minute and no test hook exists in production
//! code — the automation cannot tell who publishes clock keys.
//!
//! Determinism: state puts go through publishers that have awaited a
//! matching subscriber. A zenoh client filters puts at the writer until
//! the router's subscriber interests have propagated back to it, so a put
//! racing a freshly (re)started automation would otherwise be silently
//! dropped. Clock ticks need no such guard — the core's clock mirror
//! subscribes `home/clock/*` from the first moment. Once delivery is
//! guaranteed, same-session FIFO makes every silence assertion sound.

mod common;

use std::time::Duration;

use homeostat::bus::HealthStatus;
use serde_json::{json, Value};
use zenoh::handlers::FifoChannelHandler;
use zenoh::pubsub::Subscriber;
use zenoh::sample::Sample;

use common::{await_health, health_watch, Supervisor};

const SIM_FIXTURE: &str = "tests/fixture_house_evening_sim";
const OFF_TIME_KEY: &str = "home/config/evening_lights/off_time";
const LAMP_CMD: &str = "home/cmd/livingroom/lamp/on";
const LAMP_STATE: &str = "home/state/livingroom/lamp/on";
const PRESENCE_STATE: &str = "home/state/livingroom/presence_sensor/presence";

type Sub = Subscriber<FifoChannelHandler<Sample>>;
type Publisher = zenoh::pubsub::Publisher<'static>;

/// Declares a publisher and waits until a subscriber matches it, so
/// nothing this publisher puts is ever write-side filtered.
async fn matched_publisher(session: &zenoh::Session, key: &'static str) -> Publisher {
    let publisher = session.declare_publisher(key).await.expect("publisher");
    await_matching(&publisher).await;
    publisher
}

async fn await_matching(publisher: &Publisher) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let status = publisher
            .matching_status()
            .await
            .expect("matching status");
        if status.matching() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no subscriber matched {}",
            publisher.key_expr()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Spawns the sim fixture, waits for the automation and the reflector
/// (generous timeout: first run resolves the automation's uv env), and
/// returns matched publishers for the lamp and presence state keys.
async fn setup_sim() -> (Supervisor, zenoh::Session, Publisher, Publisher) {
    let sup = Supervisor::spawn(SIM_FIXTURE);
    let observer = sup.observer().await;
    let mut automation = health_watch(&observer, "evening_lights").await;
    await_health(&mut automation, Duration::from_secs(60), |h| {
        h.status == HealthStatus::Running
    })
    .await;
    let mut reflector = health_watch(&observer, "reflector").await;
    await_health(&mut reflector, Duration::from_secs(10), |h| {
        h.status == HealthStatus::Running
    })
    .await;
    let lamp = matched_publisher(&observer, LAMP_STATE).await;
    let presence = matched_publisher(&observer, PRESENCE_STATE).await;
    (sup, observer, lamp, presence)
}

/// Publishes a fake clock minute (RFC3339 local time, like the clock does).
async fn tick(session: &zenoh::Session, hh_mm: &str) {
    let payload = format!("\"2026-07-04T{hh_mm}:00+02:00\"");
    session
        .put("home/clock/minute", payload)
        .await
        .expect("clock tick put");
}

async fn put_state(publisher: &Publisher, value: Value) {
    publisher.put(value.to_string()).await.expect("state put");
}

/// Writes a parameter through the core's query-with-payload write path.
async fn config_write(session: &zenoh::Session, key: &str, value: Value) -> Result<Value, String> {
    let replies = session
        .get(key)
        .payload(value.to_string())
        .await
        .expect("config write query");
    let reply = replies
        .recv_async()
        .await
        .expect("config write reply");
    match reply.result() {
        Ok(sample) => Ok(serde_json::from_slice(&sample.payload().to_bytes())
            .expect("ok reply is JSON")),
        Err(err) => Err(String::from_utf8_lossy(&err.payload().to_bytes()).to_string()),
    }
}

/// Reads a concrete key from a core queryable (last-value cache).
async fn cache_read(session: &zenoh::Session, key: &str) -> Option<Value> {
    let replies = session.get(key).await.expect("cache read query");
    while let Ok(reply) = replies.recv_async().await {
        if let Ok(sample) = reply.result() {
            return Some(
                serde_json::from_slice(&sample.payload().to_bytes()).expect("reply is JSON"),
            );
        }
    }
    None
}

/// Like `cache_read`, retrying until a value shows up (for values that
/// appear once a unit has started).
async fn cache_read_eventually(session: &zenoh::Session, key: &str, timeout: Duration) -> Value {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(value) = cache_read(session, key).await {
            return value;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no value at {key} within {timeout:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Expects the lamp-off command within the timeout.
async fn expect_lamp_off(sub: &Sub, timeout: Duration) {
    let sample = tokio::time::timeout(timeout, sub.recv_async())
        .await
        .expect("lamp command within timeout")
        .expect("cmd stream open");
    assert_eq!(sample.key_expr().as_str(), LAMP_CMD);
    let value: Value =
        serde_json::from_slice(&sample.payload().to_bytes()).expect("cmd payload is JSON");
    assert_eq!(value, json!(false), "the right command is lights off");
}

/// Asserts no lamp command arrives within the window.
async fn expect_no_command(sub: &Sub, window: Duration) {
    if let Ok(Ok(sample)) = tokio::time::timeout(window, sub.recv_async()).await {
        panic!(
            "unexpected command {} = {}",
            sample.key_expr(),
            String::from_utf8_lossy(&sample.payload().to_bytes())
        );
    }
}

/// Waits until the reflector's lamp-off echo (`state on = false`) reaches
/// the observer. The echo passed the router before reaching us, so it is
/// queued to the automation ahead of anything we put afterwards — staging
/// the lamp back on after this cannot be overridden by a late echo.
async fn await_echo_off(state_sub: &Sub) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let sample = tokio::time::timeout_at(deadline, state_sub.recv_async())
            .await
            .expect("lamp state echo within 10s")
            .expect("state stream open");
        let value: Value = serde_json::from_slice(&sample.payload().to_bytes())
            .expect("state payload is JSON");
        if value == json!(false) {
            return;
        }
    }
}

/// (a) The real clock's payloads match the documented schema: RFC3339
/// local time with offset on the minute, date at the day boundary, both
/// served to late joiners by the core clock cache (no wall-clock wait).
#[tokio::test(flavor = "multi_thread")]
async fn clock_payloads_match_schema() {
    let mut sup = Supervisor::spawn("tests/fixture_house_evening");
    let observer = sup.observer().await;

    let minute = cache_read_eventually(&observer, "home/clock/minute", Duration::from_secs(60))
        .await;
    let Value::String(minute) = minute else {
        panic!("minute payload is not a JSON string: {minute}");
    };
    // 2026-07-04T21:04:00+02:00 — on the minute, offset never omitted.
    assert_eq!(minute.len(), 25, "RFC3339 with offset: {minute}");
    assert_eq!(&minute[10..11], "T", "date/time separator: {minute}");
    assert_eq!(&minute[17..19], "00", "published on the minute: {minute}");
    // The clock owns the timezone (fixture: Europe/Stockholm), so the
    // offset is CET/CEST regardless of the host timezone.
    let offset = &minute[19..];
    assert!(
        offset == "+02:00" || offset == "+01:00",
        "Europe/Stockholm offset, got {offset}"
    );

    let date = cache_read_eventually(&observer, "home/clock/date", Duration::from_secs(10)).await;
    let Value::String(date) = date else {
        panic!("date payload is not a JSON string: {date}");
    };
    assert_eq!(date, minute[..10], "date matches the minute's day");

    // The clock's own timezone parameter came through the config path.
    let timezone = cache_read(&observer, "home/config/clock/timezone").await;
    assert_eq!(timezone, Some(json!("Europe/Stockholm")));

    sup.shutdown();
}

/// (b) Presence + time crossing off_time drives the light command: no
/// command before the crossing or while someone is present; lights-off on
/// the crossing, and on presence leaving after it.
#[tokio::test(flavor = "multi_thread")]
async fn off_time_crossing_with_presence_drives_lights() {
    let (mut sup, observer, lamp, presence) = setup_sim().await;
    let cmd_sub = observer
        .declare_subscriber(LAMP_CMD)
        .await
        .expect("cmd subscriber");
    let state_sub = observer
        .declare_subscriber(LAMP_STATE)
        .await
        .expect("state subscriber");

    // Lamp on, nobody home, one minute before off_time: nothing happens.
    put_state(&lamp, json!(true)).await;
    put_state(&presence, json!(false)).await;
    tick(&observer, "21:59").await;
    expect_no_command(&cmd_sub, Duration::from_millis(1500)).await;

    // Crossing off_time (default 22:00) turns the lamp off.
    tick(&observer, "22:00").await;
    expect_lamp_off(&cmd_sub, Duration::from_secs(10)).await;
    await_echo_off(&state_sub).await;

    // Someone is home: the lamp comes back on and stays on past off_time.
    // Zenoh orders samples per key, not across keys (each subscription's
    // callbacks run on their own thread), so let presence settle — the
    // lamp is off, nothing may fire — before staging the lamp against it.
    put_state(&presence, json!(true)).await;
    expect_no_command(&cmd_sub, Duration::from_millis(1500)).await;
    put_state(&lamp, json!(true)).await;
    tick(&observer, "22:05").await;
    expect_no_command(&cmd_sub, Duration::from_millis(1500)).await;

    // They leave: lights out on the presence change, no tick needed.
    put_state(&presence, json!(false)).await;
    expect_lamp_off(&cmd_sub, Duration::from_secs(10)).await;

    sup.shutdown();
}

/// (c) A parameter edit reaches the running automation live (same pid, no
/// restart) and survives an automation restart via the core's last-value
/// cache.
#[tokio::test(flavor = "multi_thread")]
async fn off_time_edit_applies_live_and_survives_restart() {
    let (mut sup, observer, lamp, presence) = setup_sim().await;
    let cmd_sub = observer
        .declare_subscriber(LAMP_CMD)
        .await
        .expect("cmd subscriber");
    let health = cache_read(&observer, "home/health/evening_lights")
        .await
        .expect("current health served");
    let pid_before = health["pid"].as_u64().expect("running automation has a pid");

    let written = config_write(&observer, OFF_TIME_KEY, json!("23:30"))
        .await
        .expect("in-constraint write accepted");
    assert_eq!(written, json!("23:30"));

    // Let the new value settle before staging: samples are ordered per
    // key, and a 22:30 tick racing the config update on another thread
    // would fire under the old 22:00. Nothing is staged yet, so nothing
    // may fire during the settle window.
    expect_no_command(&cmd_sub, Duration::from_millis(1500)).await;

    // The old off_time (22:00) no longer fires...
    put_state(&lamp, json!(true)).await;
    put_state(&presence, json!(false)).await;
    tick(&observer, "22:30").await;
    expect_no_command(&cmd_sub, Duration::from_millis(1500)).await;

    // ...the new one does — in the same process: no restart happened.
    // (Health publishes on transitions only, so read the current state
    // from the queryable rather than waiting on the subscriber.)
    tick(&observer, "23:30").await;
    expect_lamp_off(&cmd_sub, Duration::from_secs(10)).await;
    let health = cache_read(&observer, "home/health/evening_lights")
        .await
        .expect("current health served");
    assert_eq!(health["status"], json!("running"));
    assert_eq!(health["pid"], json!(pid_before), "no unit restart on a parameter edit");

    // Kill the automation; the supervisor sweeps its process group,
    // restarts it, and the edited value still governs — last-value, not
    // manifest default.
    let mut watch = health_watch(&observer, "evening_lights").await;
    unsafe {
        libc::kill(pid_before as i32, libc::SIGKILL);
    }
    let restarted = await_health(&mut watch, Duration::from_secs(60), |h| {
        h.status == HealthStatus::Running && h.pid != Some(pid_before as u32)
    })
    .await;
    assert!(restarted.pid.is_some());

    // Wait for the fresh incarnation's subscriptions to reach this
    // session, then re-stage. The discriminator is 22:30 — silent under
    // the surviving 23:30, would fire had the value reverted to 22:00.
    await_matching(&lamp).await;
    await_matching(&presence).await;
    put_state(&lamp, json!(true)).await;
    put_state(&presence, json!(false)).await;
    tick(&observer, "22:30").await;
    expect_no_command(&cmd_sub, Duration::from_millis(1500)).await;
    tick(&observer, "23:35").await;
    expect_lamp_off(&cmd_sub, Duration::from_secs(10)).await;

    sup.shutdown();
}

/// (d) An out-of-constraint write is rejected observably — an error reply
/// naming the violation — and the effective value is unchanged: reads
/// return the old value and the automation still acts on it.
#[tokio::test(flavor = "multi_thread")]
async fn out_of_constraint_write_is_rejected() {
    let (mut sup, observer, lamp, presence) = setup_sim().await;
    let config_sub = observer
        .declare_subscriber(OFF_TIME_KEY)
        .await
        .expect("config subscriber");

    // Outside the after 20:00 / before 02:00 window.
    let err = config_write(&observer, OFF_TIME_KEY, json!("03:00"))
        .await
        .expect_err("out-of-window write rejected");
    assert!(err.contains("03:00"), "reject names the violation: {err}");
    // Wrong type and unknown parameter are rejected the same way.
    config_write(&observer, OFF_TIME_KEY, json!(42))
        .await
        .expect_err("non-time write rejected");
    config_write(&observer, "home/config/evening_lights/nope", json!("21:00"))
        .await
        .expect_err("unknown parameter rejected");

    // No rejected value ever reached the config key...
    if let Ok(Ok(sample)) =
        tokio::time::timeout(Duration::from_millis(1500), config_sub.recv_async()).await
    {
        panic!(
            "rejected write leaked onto the bus: {}",
            String::from_utf8_lossy(&sample.payload().to_bytes())
        );
    }
    // ...reads still return the old value...
    let value = cache_read(&observer, OFF_TIME_KEY).await;
    assert_eq!(value, Some(json!("22:00")), "old value stands");

    // ...and the automation still acts on it: a 22:00 crossing fires only
    // if the effective off_time is still the default.
    let cmd_sub = observer
        .declare_subscriber(LAMP_CMD)
        .await
        .expect("cmd subscriber");
    put_state(&lamp, json!(true)).await;
    put_state(&presence, json!(false)).await;
    tick(&observer, "22:00").await;
    expect_lamp_off(&cmd_sub, Duration::from_secs(10)).await;

    sup.shutdown();
}
