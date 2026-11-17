//! Integration tests for the clock service, the `evening_lights`
//! automation and the live parameter path, each scenario on a real
//! supervisor.
//!
//! The off-time-crossing scenarios run on the clock-less sim fixture. The
//! test process publishes `home/clock/minute` itself, so no scenario waits
//! out a wall-clock minute and production code needs no test hook: the
//! automation cannot tell who publishes clock keys.
//!
//! Determinism: state puts go through publishers that have awaited a
//! matching subscriber. A zenoh client filters puts at the writer until
//! the router's subscriber interests have propagated back to it, so a put
//! racing a freshly (re)started automation would otherwise be dropped
//! without notice. Clock ticks need no such guard, because the core's
//! clock mirror subscribes `home/clock/*` from the start. With delivery
//! guaranteed, per-session ordering makes every silence assertion sound.

use std::time::Duration;

use serde_json::{json, Value};

use crate::common::{
    await_matching, await_sample, cache_read, config_write, eventually, expect_event_kind,
    expect_silence, matched_publisher, next_sample, restart_unit, up, Publisher, StateSub,
    Supervisor, SILENCE,
};

const SIM_FIXTURE: &str = "tests/fixtures/house_evening_sim";
const OFF_TIME_KEY: &str = "home/config/evening_lights/off_time";
const ENABLED_KEY: &str = "home/config/evening_lights/enabled";
const LAMP_CMD: &str = "home/cmd/livingroom/lamp/on";
const LAMP_STATE: &str = "home/state/livingroom/lamp/on";
const PRESENCE_STATE: &str = "home/state/livingroom/presence_sensor/occupied";

/// Spawns the sim fixture, waits for the automation and the reflector to
/// run, and returns matched publishers for the lamp and presence state keys.
async fn setup_sim() -> (Supervisor, zenoh::Session, Publisher, Publisher) {
    let (sup, observer) = up(
        || Supervisor::spawn(SIM_FIXTURE),
        &["evening_lights", "reflector"],
    )
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

/// Expects the lamp-off command within the timeout: a cmd envelope whose
/// value is `false`, priority and actor stamped from the automation's own
/// manifest declaration (docs/design.md#cmd-envelopes). The SDK's
/// Context.publish does the stamping; the automation's code does not.
async fn expect_lamp_off(sub: &StateSub, timeout: Duration) {
    let cmd = next_sample(sub, timeout)
        .await
        .expect("lamp command within timeout");
    assert_eq!(cmd.key, LAMP_CMD);
    let envelope = cmd.value();
    let cmd_id = envelope["id"]
        .as_str()
        .expect("envelope carries a correlation id");
    assert_eq!(
        *envelope,
        json!({
            "value": false,
            "priority": "automation",
            "actor": "evening_lights",
            "id": cmd_id,
        }),
        "the right command is lights off, correctly stamped"
    );
}

/// (a) The real clock's payloads match the documented schema: RFC3339
/// local time with offset on the minute, date at the day boundary, both
/// served to late joiners by the core clock cache (no wall-clock wait).
#[tokio::test(flavor = "multi_thread")]
async fn clock_payloads_match_schema() {
    let mut sup = Supervisor::spawn("tests/fixtures/house_evening");
    let observer = sup.observer().await;

    let minute = eventually(Duration::from_mins(1), "clock minute in the mirror", || {
        cache_read(&observer, "home/clock/minute")
    })
    .await;
    let Value::String(minute) = minute else {
        panic!("minute payload is not a JSON string: {minute}");
    };
    // 2026-07-04T21:04:00+02:00: on the minute, with the offset always present.
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

    let date = eventually(Duration::from_secs(10), "clock date in the mirror", || {
        cache_read(&observer, "home/clock/date")
    })
    .await;
    let Value::String(date) = date else {
        panic!("date payload is not a JSON string: {date}");
    };
    assert_eq!(date, minute[..10], "date matches the minute's day");

    // The clock's own timezone parameter came through the config path.
    let timezone = cache_read(&observer, "home/config/clock/timezone").await;
    assert_eq!(timezone, Some(json!("Europe/Stockholm")));

    sup.shutdown();
}

/// (b) Presence + time crossing `off_time` drives the light command: no
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
    expect_silence(&cmd_sub, SILENCE, "lamp command").await;

    // Crossing off_time (default 22:00) turns the lamp off.
    tick(&observer, "22:00").await;
    expect_lamp_off(&cmd_sub, Duration::from_secs(10)).await;
    // The reflector's lamp-off echo passed the router before it reached
    // the observer, so it is queued to the automation ahead of anything put
    // after this, and a late echo cannot override staging the lamp on.
    await_sample(
        &state_sub,
        Duration::from_secs(10),
        "lamp state echo off",
        |msg| msg.value() == &json!(false),
    )
    .await;

    // Someone is home: the lamp comes back on and stays on past off_time.
    // Zenoh orders samples per key, not across keys (each subscription's
    // callbacks run on their own thread), so the test lets presence settle
    // before staging the lamp against it. The lamp is off, so nothing may
    // fire meanwhile.
    put_state(&presence, json!(true)).await;
    expect_silence(&cmd_sub, SILENCE, "lamp command").await;
    put_state(&lamp, json!(true)).await;
    tick(&observer, "22:05").await;
    expect_silence(&cmd_sub, SILENCE, "lamp command").await;

    // They leave: lights out on the presence change, no tick needed.
    put_state(&presence, json!(false)).await;
    expect_lamp_off(&cmd_sub, Duration::from_secs(10)).await;

    sup.shutdown();
}

/// (b2) A paused automation decides but sends nothing, and says so. On
/// resume the SDK delivers its inputs again, so it acts on the state the
/// house is in without waiting for the next change
/// (docs/design.md#pausing-an-automation).
#[tokio::test(flavor = "multi_thread")]
async fn a_paused_automation_sends_nothing_and_catches_up_on_resume() {
    let (mut sup, observer, lamp, presence) = setup_sim().await;
    let cmd_sub = observer
        .declare_subscriber(LAMP_CMD)
        .await
        .expect("cmd subscriber");
    let events = observer
        .declare_subscriber("home/health/evening_lights/event")
        .await
        .expect("event subscriber");

    put_state(&lamp, json!(true)).await;
    put_state(&presence, json!(false)).await;
    tick(&observer, "21:59").await;
    expect_silence(&cmd_sub, SILENCE, "lamp command").await;

    config_write(&observer, ENABLED_KEY, json!(false))
        .await
        .expect("the switch is a bool parameter every automation has");
    expect_event_kind(&events, "paused").await;

    // Crossing off_time while paused: the decision is made, the command is
    // not sent, and the decision says so (ctx.decision).
    tick(&observer, "22:00").await;
    expect_decision(&events, "turning lights off (paused)").await;
    expect_silence(&cmd_sub, SILENCE, "lamp command").await;

    // Resuming delivers the current clock minute again, which is past
    // off_time, so the lamp goes off with no new tick.
    config_write(&observer, ENABLED_KEY, json!(true))
        .await
        .expect("resume");
    expect_event_kind(&events, "resumed").await;
    expect_decision(&events, "turning lights off").await;
    expect_lamp_off(&cmd_sub, Duration::from_secs(10)).await;

    sup.shutdown();
}

/// Waits up to 10 s for a `decision` event with this outcome.
async fn expect_decision(events: &StateSub, outcome: &str) {
    let what = format!("decision \"{outcome}\"");
    let decision = await_sample(events, Duration::from_secs(10), &what, |msg| {
        let event = msg.value();
        event["kind"] == "decision" && event["outcome"] == outcome
    })
    .await;
    assert_eq!(decision.value()["rule"], "night_off");
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
    let pid_before = health["pid"]
        .as_u64()
        .expect("running automation has a pid");

    let written = config_write(&observer, OFF_TIME_KEY, json!("23:30"))
        .await
        .expect("in-constraint write accepted");
    assert_eq!(written, json!("23:30"));

    // Let the new value settle before staging: samples are ordered per
    // key, and a 22:30 tick racing the config update on another thread
    // would fire under the old 22:00. Nothing is staged yet, so nothing
    // may fire during the settle window.
    expect_silence(&cmd_sub, SILENCE, "lamp command").await;

    // The old off_time (22:00) no longer fires...
    put_state(&lamp, json!(true)).await;
    put_state(&presence, json!(false)).await;
    tick(&observer, "22:30").await;
    expect_silence(&cmd_sub, SILENCE, "lamp command").await;

    // ...the new one does, in the same process, so no restart happened.
    // (Health publishes on transitions only, so read the current state
    // from the queryable rather than waiting on the subscriber.)
    tick(&observer, "23:30").await;
    expect_lamp_off(&cmd_sub, Duration::from_secs(10)).await;
    let health = cache_read(&observer, "home/health/evening_lights")
        .await
        .expect("current health served");
    assert_eq!(health["status"], json!("running"));
    assert_eq!(
        health["pid"],
        json!(pid_before),
        "no unit restart on a parameter edit"
    );

    // Kill the automation; the supervisor sweeps its process group,
    // restarts it, and the edited value still governs: the last value wins
    // over the manifest default.
    let restarted = restart_unit(&observer, "evening_lights").await;
    assert!(restarted.pid.is_some());

    // The fresh incarnation caught up from the core mirror: it
    // already knows it is 23:30, nobody is present and the lamp is off
    // (the reflector applied the command), so it has nothing to do, and
    // staging the lamp on now would correctly be answered with lights-off.
    // The test moves the clock first, lets it settle, then re-stages. 22:30
    // tells the two apart: it is silent under the surviving 23:30 and would
    // fire had the value reverted to 22:00.
    tick(&observer, "22:30").await;
    expect_silence(&cmd_sub, SILENCE, "lamp command").await;
    await_matching(&lamp).await;
    await_matching(&presence).await;
    put_state(&lamp, json!(true)).await;
    put_state(&presence, json!(false)).await;
    expect_silence(&cmd_sub, SILENCE, "lamp command").await;
    tick(&observer, "23:35").await;
    expect_lamp_off(&cmd_sub, Duration::from_secs(10)).await;

    sup.shutdown();
}

/// (d) An out-of-constraint write gets an error reply naming the
/// violation, and the effective value is unchanged: reads
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

    // No rejected value reached the config key...
    expect_silence(&config_sub, SILENCE, "rejected write on the bus").await;
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
