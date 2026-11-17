//! Arbiter service integration tests: a real supervisor on the arbiter
//! fixture house (no broker, because the arbiter only talks on the bus),
//! asserting on the forward/preempt/refuse/expiry contract of
//! docs/design.md#arbitrated-mode.

mod common;

use std::time::Duration;

use homeostat::bus::HealthStatus;
use serde_json::{json, Value};
use zenoh::handlers::FifoChannelHandler;
use zenoh::pubsub::Subscriber;
use zenoh::sample::SampleKind;

use common::{
    assert_cli_ok, assert_unit_contract, await_health, cli, config_write, health_watch, rfc3339_in,
    running_pid, stdout, temp_house, Supervisor,
};

const FIXTURE: &str = "tests/fixtures/house_arbiter";
const CMD_KEY: &str = "home/cmd/hallway/front_door/locked";
const ARBITER_KEY: &str = "home/arbiter/hallway/front_door/locked";
const EVENT_KEY: &str = "home/health/arbiter/event";
const HOLD_MINUTES_KEY: &str = "home/config/arbiter/hold_minutes";
const HOLDS: &str = "home/hold/arbiter/**";
const HOLD_KEY: &str = "home/hold/arbiter/hallway/front_door/locked";
const SETBACK_HOLD: &str = "home/hold/setback/hallway/front_door/locked";

type Sub = Subscriber<FifoChannelHandler<zenoh::sample::Sample>>;

/// Spawns the fixture and waits for the arbiter's liveliness token.
async fn setup() -> (Supervisor, zenoh::Session) {
    let sup = Supervisor::spawn(FIXTURE);
    let observer = sup.observer().await;
    let mut watch = health_watch(&observer, "arbiter").await;
    await_health(&mut watch, Duration::from_mins(1), |h| {
        h.status == HealthStatus::Running
    })
    .await;
    (sup, observer)
}

/// The id is the publisher's correlation handle: the arbiter echoes it on
/// the events that end a command, so a refusal can be told from a timeout.
fn envelope(value: Value, priority: &str, actor: &str) -> Value {
    json!({"value": value, "priority": priority, "actor": actor, "id": "c0ffee01"})
}

async fn put_cmd(session: &zenoh::Session, payload: &Value) {
    session
        .put(CMD_KEY, payload.to_string())
        .await
        .expect("cmd put");
}

/// Next sample within the timeout, decoded as JSON.
async fn next_json(sub: &Sub, timeout: Duration) -> Option<Value> {
    let sample = tokio::time::timeout(timeout, sub.recv_async())
        .await
        .ok()?
        .expect("stream open");
    Some(serde_json::from_slice(&sample.payload().to_bytes()).expect("payload is JSON"))
}

/// The next hold sample within the timeout, as its key and the record put
/// there, or None for a delete: the hold ended and the key is gone.
async fn next_hold(sub: &Sub, timeout: Duration) -> Option<(String, Option<Value>)> {
    let sample = tokio::time::timeout(timeout, sub.recv_async())
        .await
        .ok()?
        .expect("stream open");
    let key = sample.key_expr().to_string();
    let record = match sample.kind() {
        SampleKind::Delete => None,
        SampleKind::Put => {
            Some(serde_json::from_slice(&sample.payload().to_bytes()).expect("record is JSON"))
        }
    };
    Some((key, record))
}

/// Waits for the hold on `key` to end, its key deleted.
async fn await_hold_deleted(sub: &Sub, key: &str) -> bool {
    while let Some((at, record)) = next_hold(sub, Duration::from_secs(10)).await {
        if at == key && record.is_none() {
            return true;
        }
    }
    false
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

/// What the arbiter is holding, as state rather than as an event stream:
/// one record per held aspect answers "is this aspect held right now?" for
/// a consumer that was not listening when the hold was taken
/// (docs/design.md#arbitrated-mode). Asserts the record carries the holder,
/// the value and a wall-clock deadline once a wish lands, and that the key
/// is deleted when the hold expires with no further command to prompt it.
/// Lazy expiry cannot do that last part.
#[tokio::test(flavor = "multi_thread")]
async fn holds_are_published_as_state() {
    let (mut sup, observer) = setup().await;
    let hold_sub = observer
        .declare_subscriber(HOLDS)
        .await
        .expect("hold subscriber");

    config_write(&observer, HOLD_MINUTES_KEY, json!(0.05))
        .await
        .expect("hold_minutes write");
    // The core accepts the write; the arbiter applies it when its config
    // subscription delivers, which can be after a wish sent now. A hold
    // taken at the old value runs 30 minutes, so wish again (a same-band
    // wish retakes the hold at the current value) until one is short.
    let mut held: Option<Value> = None;
    'wish: for _ in 0..10 {
        put_cmd(&observer, &envelope(json!(true), "manual", "owner")).await;
        while let Some((key, record)) = next_hold(&hold_sub, Duration::from_secs(10)).await {
            let Some(record) = record.filter(|_| key == HOLD_KEY) else {
                continue;
            };
            if hold_seconds(&record) <= 10 {
                held = Some(record);
                break 'wish;
            }
            break;
        }
    }
    let hold = held.expect("a hold record naming the manual holder");
    assert_eq!(hold["schema"], json!(1));
    assert_eq!(hold["entity"], json!("front_door"));
    assert_eq!(hold["aspect"], json!("locked"));
    assert_eq!(hold["priority"], json!("manual"));
    assert_eq!(hold["actor"], json!("owner"));
    assert_eq!(hold["value"], json!(true));
    assert_eq!(hold["refused"], json!(0));
    // Wall clock, because a monotonic deadline means nothing off-process.
    let until = hold["until"].as_str().expect("until is a string");
    assert!(
        until.ends_with('Z') && until.len() >= 20,
        "until should be RFC3339 UTC, got {until:?}"
    );

    // The key goes at the deadline. Nothing commands the arbiter here:
    // lazy expiry would leave the record claiming a hold that had ended,
    // which is the failure this test exists for.
    assert!(
        await_hold_deleted(&hold_sub, HOLD_KEY).await,
        "the hold record should be deleted at expiry"
    );

    assert_unit_contract(&mut sup, &observer, "arbiter").await;
    sup.shutdown();
}

/// How long a published hold runs, from its RFC3339 `since` and `until`
/// (`...THH:MM:SSZ`); a hold is shorter than a day.
fn hold_seconds(hold: &Value) -> i64 {
    let second_of_day = |field: &str| -> i64 {
        let stamp = hold[field].as_str().expect("an RFC3339 stamp");
        stamp[11..19]
            .split(':')
            .map(|part| part.parse::<i64>().expect("HH:MM:SS"))
            .fold(0, |acc, part| acc * 60 + part)
    };
    (second_of_day("until") - second_of_day("since")).rem_euclid(86_400)
}

/// Waits until the arbiter takes holds at the `hold_minutes` just written.
/// The core accepts a write before the arbiter's config subscription
/// delivers it, so a wish sent straight after can be held at the old
/// value. This probes a sibling aspect the caller does not use, re-wishing
/// (each wish retakes the hold at the current value) until the hold it
/// gets runs at most `max_s` seconds.
async fn await_hold_minutes_applied(observer: &zenoh::Session, max_s: i64) {
    const PROBE: &str = "home/cmd/hallway/front_door/auto_relock_delay";
    let hold_sub = observer
        .declare_subscriber(HOLDS)
        .await
        .expect("hold subscriber");
    for _ in 0..20 {
        observer
            .put(PROBE, envelope(json!(1), "manual", "probe").to_string())
            .await
            .expect("probe put");
        while let Some((_key, record)) = next_hold(&hold_sub, Duration::from_secs(2)).await {
            if let Some(hold) = record.filter(|h| h["actor"] == json!("probe")) {
                if hold_seconds(&hold) <= max_s {
                    return;
                }
                break;
            }
        }
    }
    panic!("the arbiter never took a hold at the written hold_minutes");
}

/// A hold counts what it refuses: the tally rides the hold itself, so a
/// consumer can say what an override cost without replaying the events.
#[tokio::test(flavor = "multi_thread")]
async fn a_hold_counts_what_it_refuses() {
    let (mut sup, observer) = setup().await;
    let hold_sub = observer
        .declare_subscriber(HOLDS)
        .await
        .expect("hold subscriber");

    put_cmd(&observer, &envelope(json!(true), "manual", "owner")).await;
    put_cmd(
        &observer,
        &envelope(json!(false), "automation", "scheduler"),
    )
    .await;

    let mut refused = None;
    for _ in 0..6 {
        let Some((_key, record)) = next_hold(&hold_sub, Duration::from_secs(10)).await else {
            break;
        };
        let count = record.map_or(Value::Null, |r| r["refused"].clone());
        if count == json!(1) {
            refused = Some(count);
            break;
        }
    }
    assert_eq!(
        refused,
        Some(json!(1)),
        "the manual hold should count the automation wish it turned away"
    );

    assert_unit_contract(&mut sup, &observer, "arbiter").await;
    sup.shutdown();
}

/// A restarted arbiter reads the applied hold back from its arbitration
/// document, so a unit's hold that was already sent is not sent again
/// (docs/design.md#which-hold-wins).
#[tokio::test(flavor = "multi_thread")]
async fn a_restarted_arbiter_sends_nothing_already_applied() {
    let (mut sup, observer) = setup().await;
    let arbiter_sub = observer
        .declare_subscriber(ARBITER_KEY)
        .await
        .expect("arbiter subscriber");
    observer
        .put(
            SETBACK_HOLD,
            json!({"value": false, "priority": "automation", "id": "5e7bac02"}).to_string(),
        )
        .await
        .expect("hold put");
    assert!(
        next_json(&arbiter_sub, Duration::from_secs(10))
            .await
            .is_some(),
        "the hold is applied"
    );
    // The document carries it before the restart.
    let mut documented = false;
    for _ in 0..50 {
        let doc = common::cache_read(&observer, "home/arbitration/arbiter").await;
        if doc.is_some_and(|d| d["aspects"][0]["applied"]["id"] == json!("5e7bac02")) {
            documented = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        documented,
        "the arbitration document names the applied hold"
    );

    let pid = running_pid(&observer, "arbiter").await;
    let mut watch = health_watch(&observer, "arbiter").await;
    unsafe {
        libc::kill(i32::try_from(pid).expect("a pid fits i32"), libc::SIGKILL);
    }
    await_health(&mut watch, Duration::from_mins(1), |h| {
        h.status == HealthStatus::Running && h.pid != Some(u32::try_from(pid).expect("pid fits"))
    })
    .await;
    expect_silence(
        &arbiter_sub,
        Duration::from_secs(2),
        "a forward of the hold already applied",
    )
    .await;

    // A changed hold is still sent.
    observer
        .put(
            SETBACK_HOLD,
            json!({"value": true, "priority": "automation", "id": "5e7bac03"}).to_string(),
        )
        .await
        .expect("hold put");
    let forwarded = next_json(&arbiter_sub, Duration::from_secs(10))
        .await
        .expect("the changed hold is sent");
    assert_eq!(forwarded["id"], json!("5e7bac03"));

    sup.shutdown();
}

/// A holder reported `stopped` is only away; once its breaker opens it is
/// gone, and its hold ends unless it is for the rest value or its publish
/// says `keep`, so the hold beneath is sent
/// (docs/design.md#when-a-holder-stops).
#[tokio::test(flavor = "multi_thread")]
async fn a_gone_holders_hold_ends_and_the_one_beneath_is_sent() {
    let house = temp_house(FIXTURE, "arbiter-gone");
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("adapters/arbiter.py");
    edit(
        &house,
        "units/arbiter.toml",
        "uv run ../../../adapters/arbiter.py",
        &format!("uv run {}", script.display()),
    );
    // A second holder at agent, which exits whenever the test asks it to.
    std::fs::write(
        house.join("units/curfew.toml"),
        "schema = 1\n\n[unit]\nname = \"curfew\"\nkind = \"automation\"\n\n\
         [runtime]\ncommand = \"fake_adapter --crash-key home/cmd/test/curfew/crash\"\n\
         restart = \"always\"\nshutdown_grace_s = 5\n\n\
         [bus.publishes]\nlocked = { key = \"home/hold/curfew/hallway/front_door/locked\", \
         capability = \"lock\", priority = \"agent\" }\n",
    )
    .expect("write the curfew unit");
    let mut sup = Supervisor::spawn_at(&house, &[]);
    let observer = sup.observer().await;
    let mut watch = health_watch(&observer, "arbiter").await;
    await_health(&mut watch, Duration::from_mins(1), |h| {
        h.status == HealthStatus::Running
    })
    .await;
    let mut curfew = health_watch(&observer, "curfew").await;
    await_health(&mut curfew, Duration::from_mins(1), |h| {
        h.status == HealthStatus::Running
    })
    .await;
    let arbiter_sub = observer
        .declare_subscriber(ARBITER_KEY)
        .await
        .expect("arbiter subscriber");

    observer
        .put(
            SETBACK_HOLD,
            json!({"value": false, "priority": "automation", "id": "5e7bac04"}).to_string(),
        )
        .await
        .expect("hold put");
    assert_eq!(
        next_json(&arbiter_sub, Duration::from_secs(10))
            .await
            .map(|f| f["id"].clone()),
        Some(json!("5e7bac04"))
    );
    observer
        .put(
            "home/hold/curfew/hallway/front_door/locked",
            json!({"value": true, "priority": "agent", "id": "c0f00001"}).to_string(),
        )
        .await
        .expect("hold put");
    assert_eq!(
        next_json(&arbiter_sub, Duration::from_secs(10))
            .await
            .map(|f| f["id"].clone()),
        Some(json!("c0f00001")),
        "the agent's hold wins"
    );

    // `stopped` is what an apply's restart and a shutdown report: the
    // holder is away, not gone, and nothing moves.
    observer
        .put(
            "home/health/curfew",
            json!({"status": "stopped", "pid": null, "restarts": 0}).to_string(),
        )
        .await
        .expect("health put");
    expect_silence(
        &arbiter_sub,
        Duration::from_millis(800),
        "a forward for a holder briefly away",
    )
    .await;

    // Crashed until its breaker opens, the holder is gone. It is running at
    // the start; after each crash the supervisor brings it back.
    let mut opened = false;
    for round in 0..6 {
        if round > 0 {
            await_health(&mut curfew, Duration::from_mins(1), |h| {
                h.status == HealthStatus::Running
            })
            .await;
        }
        // The token can be up before the crash subscription is, so ask
        // until it exits.
        let mut exit = None;
        for _ in 0..40 {
            observer
                .put("home/cmd/test/curfew/crash", "1")
                .await
                .expect("crash put");
            exit = common::scan_health(&mut curfew, Duration::from_millis(500), |h| {
                h.status != HealthStatus::Running
            })
            .await;
            if exit.is_some() {
                break;
            }
        }
        let exit = exit.expect("the curfew unit exits when asked");
        if exit.status == HealthStatus::Open {
            opened = true;
            break;
        }
    }
    assert!(opened, "the breaker opens");
    let forwarded = next_json(&arbiter_sub, Duration::from_secs(10))
        .await
        .expect("the hold beneath is sent");
    assert_eq!(
        forwarded["id"],
        json!("5e7bac04"),
        "a lock has no rest value, so the gone curfew's hold ended"
    );

    sup.shutdown();
}

/// A hold the adapter could not deliver, because the device was away, is
/// sent again when the device's `available` returns. A delivered one is
/// not (docs/design.md#when-a-holder-stops).
#[tokio::test(flavor = "multi_thread")]
async fn an_undelivered_hold_is_sent_when_the_device_is_back() {
    let (mut sup, observer) = setup().await;
    let arbiter_sub = observer
        .declare_subscriber(ARBITER_KEY)
        .await
        .expect("arbiter subscriber");
    observer
        .put(
            SETBACK_HOLD,
            json!({"value": false, "priority": "automation", "id": "5e7bac05"}).to_string(),
        )
        .await
        .expect("hold put");
    assert!(next_json(&arbiter_sub, Duration::from_secs(10))
        .await
        .is_some());

    // Back without a failed delivery: nothing is sent.
    let available = "home/state/hallway/front_door/available";
    observer
        .put(available, "true")
        .await
        .expect("available put");
    expect_silence(
        &arbiter_sub,
        Duration::from_millis(800),
        "a forward of a delivered hold",
    )
    .await;

    // The adapter reports the forward undelivered, then the device returns.
    observer
        .put(
            "home/health/reflector/event",
            json!({"kind": "drop", "reason": "device-unavailable", "cmd_id": "5e7bac05"})
                .to_string(),
        )
        .await
        .expect("event put");
    tokio::time::sleep(Duration::from_millis(300)).await;
    observer
        .put(available, "true")
        .await
        .expect("available put");
    let forwarded = next_json(&arbiter_sub, Duration::from_secs(10))
        .await
        .expect("the undelivered hold is sent again");
    assert_eq!(forwarded["id"], json!("5e7bac05"));

    sup.shutdown();
}

/// A manual command's `hold` decides what happens to the family hold
/// (docs/design.md#hold-intents): "none" forwards and holds nothing,
/// "report" holds without forwarding, "release" hands the aspect back
/// without forwarding, and an instant holds until then. A hold on a lower
/// band's command is malformed.
#[tokio::test(flavor = "multi_thread")]
async fn hold_intents_steer_the_family_hold() {
    let (mut sup, observer) = setup().await;
    let arbiter_sub = observer
        .declare_subscriber(ARBITER_KEY)
        .await
        .expect("arbiter subscriber");
    let hold_sub = observer
        .declare_subscriber(HOLDS)
        .await
        .expect("hold subscriber");
    let event_sub = observer
        .declare_subscriber(EVENT_KEY)
        .await
        .expect("event subscriber");
    let with_hold = |value: Value, hold: &str| {
        let mut wish = envelope(value, "manual", "owner");
        wish["hold"] = json!(hold);
        wish
    };
    let auto_wish = envelope(json!(true), "automation", "scheduler");

    // "none": the device is asked, and nothing is held at the manual band,
    // so an automation's next wish goes through.
    let nudge = with_hold(json!(false), "none");
    put_cmd(&observer, &nudge).await;
    let forwarded = next_json(&arbiter_sub, Duration::from_secs(10)).await;
    assert_eq!(forwarded, Some(nudge), "a nudge is forwarded unchanged");
    put_cmd(&observer, &auto_wish).await;
    let forwarded = next_json(&arbiter_sub, Duration::from_secs(10)).await;
    assert_eq!(
        forwarded,
        Some(auto_wish.clone()),
        "nothing held the aspect"
    );

    // "report": the device already did it. The family hold is taken, and
    // nothing is forwarded.
    put_cmd(&observer, &with_hold(json!(false), "report")).await;
    let mut record = None;
    while let Some((key, put)) = next_hold(&hold_sub, Duration::from_secs(10)).await {
        if key == HOLD_KEY
            && put
                .as_ref()
                .is_some_and(|r| r["priority"] == json!("manual"))
        {
            record = put;
            break;
        }
    }
    let record = record.expect("a family hold record");
    assert_eq!(
        record["value"],
        json!(false),
        "the hold records what the device did"
    );
    expect_silence(
        &arbiter_sub,
        Duration::from_millis(500),
        "forward of a report",
    )
    .await;
    put_cmd(&observer, &auto_wish).await;
    let event = next_json(&event_sub, Duration::from_secs(10))
        .await
        .expect("refuse event");
    assert_eq!(event["kind"], json!("refuse"));

    // "release": no value, nothing forwarded, the hold is gone, and the
    // automation is heard again.
    let mut release = json!({"priority": "manual", "actor": "owner", "hold": "release"});
    release["id"] = json!("c0ffee02");
    put_cmd(&observer, &release).await;
    assert!(
        await_hold_deleted(&hold_sub, HOLD_KEY).await,
        "a release deletes the hold"
    );
    expect_silence(
        &arbiter_sub,
        Duration::from_millis(500),
        "forward of a release",
    )
    .await;
    put_cmd(&observer, &auto_wish).await;
    let forwarded = next_json(&arbiter_sub, Duration::from_secs(10)).await;
    assert_eq!(
        forwarded,
        Some(auto_wish),
        "a released aspect is open again"
    );

    // An instant: forwarded, and held until then.
    let instant = rfc3339_in(86_400);
    let until = with_hold(json!(true), &instant);
    put_cmd(&observer, &until).await;
    let forwarded = next_json(&arbiter_sub, Duration::from_secs(10)).await;
    assert_eq!(forwarded, Some(until));
    let mut held_until = None;
    while let Some((key, put)) = next_hold(&hold_sub, Duration::from_secs(10)).await {
        if let Some(r) = put.filter(|r| key == HOLD_KEY && r["priority"] == json!("manual")) {
            held_until = Some(r["until"].clone());
            break;
        }
    }
    assert_eq!(held_until, Some(json!(instant)));

    // A hold below the manual band is a bug in its publisher.
    let mut bad = envelope(json!(true), "automation", "scheduler");
    bad["hold"] = json!("none");
    put_cmd(&observer, &bad).await;
    // Skip whatever else the steps above reported.
    let mut dropped = None;
    while let Some(event) = next_json(&event_sub, Duration::from_secs(10)).await {
        if event["kind"] == json!("drop") {
            dropped = Some(event);
            break;
        }
    }
    let dropped = dropped.expect("drop event");
    assert_eq!(dropped["reason"], json!("invalid-command"));

    sup.shutdown();
}

/// The family's holds outlive the arbiter process: a restarted arbiter
/// reads them back from the core's mirror and goes on refusing what they
/// refused.
#[tokio::test(flavor = "multi_thread")]
async fn holds_survive_an_arbiter_restart() {
    let (mut sup, observer) = setup().await;
    let hold_sub = observer
        .declare_subscriber(HOLDS)
        .await
        .expect("hold subscriber");
    let event_sub = observer
        .declare_subscriber(EVENT_KEY)
        .await
        .expect("event subscriber");

    put_cmd(&observer, &envelope(json!(true), "manual", "owner")).await;
    let mut taken = false;
    while let Some((key, put)) = next_hold(&hold_sub, Duration::from_secs(10)).await {
        if key == HOLD_KEY && put.is_some() {
            taken = true;
            break;
        }
    }
    assert!(taken, "the manual wish takes a hold");

    let pid = running_pid(&observer, "arbiter").await;
    let mut watch = health_watch(&observer, "arbiter").await;
    unsafe {
        libc::kill(i32::try_from(pid).expect("a pid fits i32"), libc::SIGKILL);
    }
    await_health(&mut watch, Duration::from_mins(1), |h| {
        h.status == HealthStatus::Running && h.pid != Some(u32::try_from(pid).expect("pid fits"))
    })
    .await;

    put_cmd(
        &observer,
        &envelope(json!(false), "automation", "scheduler"),
    )
    .await;
    let mut refusal = None;
    while let Some(event) = next_json(&event_sub, Duration::from_secs(10)).await {
        if event["kind"] == json!("refuse") {
            refusal = Some(event);
            break;
        }
    }
    let refusal = refusal.expect("the restored hold refuses the automation");
    assert_eq!(refusal["holder_actor"], json!("owner"));
    // The restart put nothing back and deleted nothing: the first sample
    // since is the refusal's count on the record that was kept.
    let (key, record) = next_hold(&hold_sub, Duration::from_secs(10))
        .await
        .expect("the refusal is counted on the hold");
    assert_eq!(key, HOLD_KEY);
    assert_eq!(record.expect("a put, not a delete")["refused"], json!(1));

    sup.shutdown();
}

/// A unit's hold and the family's on one arbitrated aspect
/// (docs/design.md#which-hold-wins): a command with nothing held forwards
/// and holds nothing; a unit's hold is forwarded as soon as it is placed;
/// the family's hold masks it (preempt), refuses the unit's commands, and
/// when it ends the unit's hold is sent again (resume) without anyone
/// sending; at the unit's band a command must agree with its hold.
#[tokio::test(flavor = "multi_thread")]
async fn a_units_hold_is_masked_by_the_family_and_sent_when_it_ends() {
    let (mut sup, observer) = setup().await;
    let arbiter_sub = observer
        .declare_subscriber(ARBITER_KEY)
        .await
        .expect("arbiter subscriber");
    let event_sub = observer
        .declare_subscriber(EVENT_KEY)
        .await
        .expect("event subscriber");

    // A command with nothing held forwards unchanged, options included.
    let mut auto_wish = envelope(json!(true), "automation", "scheduler");
    auto_wish["options"] = json!({"transition": 1.0});
    put_cmd(&observer, &auto_wish).await;
    let forwarded = next_json(&arbiter_sub, Duration::from_secs(10)).await;
    assert_eq!(forwarded, Some(auto_wish), "envelope forwarded unchanged");
    expect_silence(
        &event_sub,
        Duration::from_millis(500),
        "event on a free aspect",
    )
    .await;

    // The unit's hold is sent at once, under its name and id.
    observer
        .put(
            SETBACK_HOLD,
            json!({"value": false, "priority": "automation", "id": "5e7bac01"}).to_string(),
        )
        .await
        .expect("hold put");
    let applied =
        json!({"value": false, "priority": "automation", "actor": "setback", "id": "5e7bac01"});
    let forwarded = next_json(&arbiter_sub, Duration::from_secs(10)).await;
    assert_eq!(forwarded, Some(applied.clone()), "the hold is applied");

    // A short family hold masks it.
    config_write(&observer, HOLD_MINUTES_KEY, json!(0.02))
        .await
        .expect("write accepted");
    await_hold_minutes_applied(&observer, 5).await;
    let manual_wish = envelope(json!(true), "manual", "owner");
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
            "from_actor": "setback",
            "to_priority": "manual",
            "to_actor": "owner",
        })
    );
    let forwarded = next_json(&arbiter_sub, Duration::from_secs(10)).await;
    assert_eq!(
        forwarded,
        Some(manual_wish),
        "the family's command is sent itself"
    );

    // While it stands, a command below it is refused.
    put_cmd(
        &observer,
        &envelope(json!(false), "automation", "scheduler"),
    )
    .await;
    let event = next_json(&event_sub, Duration::from_secs(10))
        .await
        .expect("refuse event");
    assert_eq!(event["kind"], json!("refuse"));
    assert_eq!(event["holder_actor"], json!("owner"));

    // When it ends, the unit's hold is sent again: nobody sends anything.
    let event = next_json(&event_sub, Duration::from_secs(10))
        .await
        .expect("resume event");
    assert_eq!(event["kind"], json!("resume"));
    assert_eq!(event["to_actor"], json!("setback"));
    let forwarded = next_json(&arbiter_sub, Duration::from_secs(10)).await;
    assert_eq!(
        forwarded,
        Some(applied),
        "the hold beneath is sent when the family's ends"
    );

    // At the hold's own band a command must agree with it.
    put_cmd(&observer, &envelope(json!(true), "automation", "scheduler")).await;
    let event = next_json(&event_sub, Duration::from_secs(10))
        .await
        .expect("refuse event");
    assert_eq!(event["holder_actor"], json!("setback"));
    let agreeing = envelope(json!(false), "automation", "scheduler");
    put_cmd(&observer, &agreeing).await;
    assert_eq!(
        next_json(&arbiter_sub, Duration::from_secs(10)).await,
        Some(agreeing)
    );

    // Released, nothing stands, and nothing is sent.
    observer.delete(SETBACK_HOLD).await.expect("hold delete");
    expect_silence(
        &arbiter_sub,
        Duration::from_millis(800),
        "a forward on release",
    )
    .await;

    sup.shutdown();
}

/// Leases are per (entity, aspect), not per entity
/// (docs/design.md#arbitrated-mode): a manual hold on one aspect must not
/// block an automation commanding a sibling aspect of the same entity, so
/// the family's setpoint does not freeze the price automation's offset.
/// Same-aspect contention still refuses.
#[tokio::test(flavor = "multi_thread")]
async fn aspects_lease_independently() {
    let (mut sup, observer) = setup().await;
    let sibling_cmd = "home/cmd/hallway/front_door/auto_relock_delay";
    let sibling_arbiter = "home/arbiter/hallway/front_door/auto_relock_delay";
    let locked_sub = observer
        .declare_subscriber(ARBITER_KEY)
        .await
        .expect("locked subscriber");
    let sibling_sub = observer
        .declare_subscriber(sibling_arbiter)
        .await
        .expect("sibling subscriber");
    let event_sub = observer
        .declare_subscriber(EVENT_KEY)
        .await
        .expect("event subscriber");

    // Manual takes the lease on "locked" (fixture default hold_minutes is
    // long-lived; the hold outlasts this test).
    let manual_wish = envelope(json!(true), "manual", "owner");
    put_cmd(&observer, &manual_wish).await;
    let forwarded = next_json(&locked_sub, Duration::from_secs(10))
        .await
        .expect("manual wish forwarded");
    assert_eq!(forwarded, manual_wish);

    // An automation wish on a sibling aspect of the same entity flows: it
    // takes its own lease, with no refusal and no event.
    let sibling_wish = envelope(json!(30), "automation", "scheduler");
    observer
        .put(sibling_cmd, sibling_wish.to_string())
        .await
        .expect("sibling cmd put");
    let forwarded = next_json(&sibling_sub, Duration::from_secs(10))
        .await
        .expect("sibling-aspect automation wish forwarded despite manual hold on locked");
    assert_eq!(forwarded, sibling_wish);
    expect_silence(
        &event_sub,
        Duration::from_millis(500),
        "event on an independent aspect",
    )
    .await;

    // Same-aspect contention still arbitrates: automation on "locked" is
    // refused under the manual hold.
    put_cmd(
        &observer,
        &envelope(json!(false), "automation", "scheduler"),
    )
    .await;
    let event = next_json(&event_sub, Duration::from_secs(10))
        .await
        .expect("refuse event");
    assert_eq!(event["kind"], json!("refuse"));
    assert_eq!(event["aspect"], json!("locked"));
    expect_silence(
        &locked_sub,
        Duration::from_millis(500),
        "forward of a refused wish",
    )
    .await;

    sup.shutdown();
}

/// (f) A malformed cmd envelope for an arbitrated entity drops with an
/// "invalid-command" health event, and does not reach home/arbiter/**.
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

    // A bare value is not an envelope.
    observer.put(CMD_KEY, "true").await.expect("cmd put");
    let event = next_json(&event_sub, Duration::from_secs(10))
        .await
        .expect("invalid-command event");
    assert_eq!(event["kind"], json!("drop"));
    assert_eq!(event["reason"], json!("invalid-command"));
    expect_silence(
        &arbiter_sub,
        Duration::from_millis(1500),
        "forward of a malformed command",
    )
    .await;

    // An envelope with an unknown priority is malformed too, but it parsed
    // far enough to carry an id, and the drop reports it. The publisher
    // learns its command died here instead of waiting out a timeout. The
    // malformed-payload case above has no id to report: nothing parsed.
    observer
        .put(
            CMD_KEY,
            json!({"value": true, "priority": "urgent", "actor": "x", "id": "badbeef0"})
                .to_string(),
        )
        .await
        .expect("cmd put");
    let event = next_json(&event_sub, Duration::from_secs(10))
        .await
        .expect("invalid-command event");
    assert_eq!(event["reason"], json!("invalid-command"));
    assert_eq!(event["cmd_id"], json!("badbeef0"));

    // Still alive and translating afterwards.
    let ok_wish = envelope(json!(true), "automation", "scheduler");
    put_cmd(&observer, &ok_wish).await;
    let forwarded = next_json(&arbiter_sub, Duration::from_secs(10))
        .await
        .expect("a well-formed wish still forwards");
    assert_eq!(forwarded, ok_wish);

    sup.shutdown();
}

/// A wish for an entity no unit binds drops with an "unbound" health
/// event. No adapter consumes it either, so without the event it would
/// vanish without a trace.
#[tokio::test(flavor = "multi_thread")]
async fn wish_for_an_unbound_entity_drops_with_health_event() {
    let (mut sup, observer) = setup().await;
    let event_sub = observer
        .declare_subscriber(EVENT_KEY)
        .await
        .expect("event subscriber");

    observer
        .put(
            "home/cmd/hallway/back_door/locked",
            envelope(json!(true), "automation", "scheduler").to_string(),
        )
        .await
        .expect("cmd put");
    let event = next_json(&event_sub, Duration::from_secs(10))
        .await
        .expect("unbound event");
    assert_eq!(event["kind"], json!("drop"));
    assert_eq!(event["reason"], json!("unbound"));
    assert_eq!(event["cmd_id"], json!("c0ffee01"));

    sup.shutdown();
}

/// An apply that makes an entity arbitrated restarts its adapter but not
/// the arbiter, whose files are unchanged. The arbiter follows the grant
/// table the core republishes, so the next wish forwards without a
/// restart. Flipping the entity back releases the hold that wish took.
#[tokio::test(flavor = "multi_thread")]
async fn an_entity_made_arbitrated_by_apply_forwards_without_a_restart() {
    let house = temp_house(FIXTURE, "arbiter-flip");
    // A hold grant on a shared lock is a plan error, so the holder goes.
    std::fs::remove_file(house.join("units/setback.toml")).expect("remove the holder");
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("adapters/arbiter.py");
    edit(
        &house,
        "units/arbiter.toml",
        "uv run ../../../adapters/arbiter.py",
        &format!("uv run {}", script.display()),
    );
    edit(
        &house,
        "entities/reflector/front_door.toml",
        "mode = \"arbitrated\"",
        "mode = \"shared\"",
    );
    let mut sup = Supervisor::spawn_at(&house, &[]);
    let observer = sup.observer().await;
    let mut watch = health_watch(&observer, "arbiter").await;
    await_health(&mut watch, Duration::from_mins(1), |h| {
        h.status == HealthStatus::Running
    })
    .await;
    let arbiter_pid = running_pid(&observer, "arbiter").await;
    let arbiter_sub = observer
        .declare_subscriber(ARBITER_KEY)
        .await
        .expect("arbiter subscriber");

    // Shared: the adapter consumes the wish, and the arbiter stays out.
    put_cmd(&observer, &envelope(json!(true), "automation", "scheduler")).await;
    expect_silence(
        &arbiter_sub,
        Duration::from_millis(1500),
        "forward of a shared entity's wish",
    )
    .await;

    edit(
        &house,
        "entities/reflector/front_door.toml",
        "mode = \"shared\"",
        "mode = \"arbitrated\"",
    );
    let house_arg = house.to_str().expect("utf-8 path");
    let apply = cli(&["apply", house_arg, "--bus", &sup.endpoint]);
    assert_cli_ok(&apply);
    let text = stdout(&apply);
    assert!(text.contains("restart reflector: ok"), "{text}");
    assert!(!text.contains("restart arbiter"), "{text}");

    // The core puts the table before apply returns, but the arbiter's
    // subscription delivers it asynchronously, so wish until one forwards.
    let wish = envelope(json!(true), "automation", "scheduler");
    let mut forwarded = None;
    for _ in 0..20 {
        put_cmd(&observer, &wish).await;
        forwarded = next_json(&arbiter_sub, Duration::from_millis(500)).await;
        if forwarded.is_some() {
            break;
        }
    }
    assert_eq!(forwarded, Some(wish), "the wish should forward after apply");
    assert_eq!(
        running_pid(&observer, "arbiter").await,
        arbiter_pid,
        "the arbiter was not restarted"
    );

    // A family wish takes a 30-minute hold. Once the entity is shared
    // again its adapter takes commands directly, so the hold must go.
    let family = envelope(json!(false), "manual", "owner");
    put_cmd(&observer, &family).await;
    assert_eq!(
        next_json(&arbiter_sub, Duration::from_secs(10)).await,
        Some(family),
        "the family's wish is forwarded and holds"
    );
    let hold_sub = observer
        .declare_subscriber(HOLDS)
        .await
        .expect("hold subscriber");
    edit(
        &house,
        "entities/reflector/front_door.toml",
        "mode = \"arbitrated\"",
        "mode = \"shared\"",
    );
    let apply = cli(&["apply", house_arg, "--bus", &sup.endpoint]);
    assert_cli_ok(&apply);
    let released = await_hold_deleted(&hold_sub, HOLD_KEY).await;
    assert!(
        released,
        "the hold should be released when the entity is shared"
    );

    sup.shutdown();
    let _ = std::fs::remove_dir_all(&house);
}

/// The core advances the applied grant table only when an apply completes,
/// but the walk restarts the entity's adapter partway through. A wish sent
/// in between must still reach the device: the arbiter routes on the table
/// the walk is moving to as well, which the core publishes when the walk
/// starts and empties when the apply completes (docs/design.md#arbitrated-mode).
/// A unit that is slow to become ready holds the walk open after the adapter.
#[tokio::test(flavor = "multi_thread")]
async fn a_wish_during_the_apply_that_arbitrates_an_entity_forwards() {
    let house = temp_house(FIXTURE, "arbiter-walk");
    // A hold grant on a shared lock is a plan error, so the holder goes.
    std::fs::remove_file(house.join("units/setback.toml")).expect("remove the holder");
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("adapters/arbiter.py");
    edit(
        &house,
        "units/arbiter.toml",
        "uv run ../../../adapters/arbiter.py",
        &format!("uv run {}", script.display()),
    );
    edit(
        &house,
        "entities/reflector/front_door.toml",
        "mode = \"arbitrated\"",
        "mode = \"shared\"",
    );
    let mut sup = Supervisor::spawn_at(&house, &[]);
    let observer = sup.observer().await;
    let mut watch = health_watch(&observer, "arbiter").await;
    await_health(&mut watch, Duration::from_mins(1), |h| {
        h.status == HealthStatus::Running
    })
    .await;
    let reflector_pid = running_pid(&observer, "reflector").await;
    let arbiter_sub = observer
        .declare_subscriber(ARBITER_KEY)
        .await
        .expect("arbiter subscriber");

    edit(
        &house,
        "entities/reflector/front_door.toml",
        "mode = \"shared\"",
        "mode = \"arbitrated\"",
    );
    std::fs::write(
        house.join("units/slow.toml"),
        "schema = 1\n\n[unit]\nname = \"slow\"\nkind = \"service\"\n\
         description = \"Holds the walk open after the adapter\"\n\n[runtime]\n\
         command = \"fake_adapter --ready-after-ms 8000 \
         --state-key home/state/testroom/slow/value --crash-key home/cmd/testroom/slow/crash\"\n\
         restart = \"always\"\nshutdown_grace_s = 5\n",
    )
    .expect("write the slow unit");
    let house_arg = house.to_str().expect("utf-8 path").to_string();
    let endpoint = sup.endpoint.clone();
    let apply =
        tokio::task::spawn_blocking(move || cli(&["apply", &house_arg, "--bus", &endpoint]));

    // Wait for the adapter to come back on the new table, while the walk
    // waits on the slow unit.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let health = observer.get(homeostat::bus::health_key("reflector")).await;
        let restarted = match health {
            Ok(replies) => {
                let mut pid = None;
                while let Ok(reply) = replies.recv_async().await {
                    if let Ok(sample) = reply.result() {
                        let h: Value = serde_json::from_slice(&sample.payload().to_bytes())
                            .unwrap_or(Value::Null);
                        if h["status"] == json!("running") {
                            pid = h["pid"].as_u64();
                        }
                    }
                }
                pid.is_some_and(|pid| pid != reflector_pid)
            }
            Err(_) => false,
        };
        if restarted {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the reflector never restarted"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let wish = envelope(json!(true), "automation", "scheduler");
    put_cmd(&observer, &wish).await;
    let forwarded = next_json(&arbiter_sub, Duration::from_secs(3)).await;
    assert!(
        !apply.is_finished(),
        "the walk ended before the wish was checked"
    );
    assert_eq!(
        forwarded,
        Some(wish),
        "a wish sent during the walk forwards"
    );

    let apply = apply.await.expect("apply task");
    assert_cli_ok(&apply);
    let replies = observer
        .get("home/meta/system/grants_pending")
        .await
        .expect("pending get");
    let reply = replies.recv_async().await.expect("a pending reply");
    let pending: Value =
        serde_json::from_slice(&reply.result().expect("an ok reply").payload().to_bytes())
            .expect("JSON");
    assert_eq!(pending, json!([]), "the pending table empties at the end");

    sup.shutdown();
    let _ = std::fs::remove_dir_all(&house);
}

/// Replaces `from` with `to` in a house file; the pattern must be present.
fn edit(house: &std::path::Path, rel: &str, from: &str, to: &str) {
    let path = house.join(rel);
    let text = std::fs::read_to_string(&path).expect("read house file");
    assert!(text.contains(from), "{rel} does not contain {from:?}");
    std::fs::write(&path, text.replace(from, to)).expect("write house file");
}

/// (e) The arbiter honors the unit contract: liveliness token when
/// ready, clean SIGTERM shutdown within the grace, no orphans.
#[tokio::test(flavor = "multi_thread")]
async fn adapter_honors_unit_contract() {
    let (mut sup, observer) = setup().await;
    assert_unit_contract(&mut sup, &observer, "arbiter").await;
}
