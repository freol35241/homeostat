//! Arbiter service integration tests: a real supervisor on the arbiter
//! fixture house (no broker, because the arbiter only talks on the bus),
//! asserting on the forward/preempt/refuse/expiry contract of
//! docs/design.md#arbitrated-mode.

use crate::common;

use std::time::Duration;

use homeostat::bus::HealthStatus;
use serde_json::{json, Value};

use crate::common::{
    assert_cli_ok, assert_unit_contract, await_health, await_sample, cache_read, cli, config_write,
    edit, eventually, expect_silence, health_watch, next_sample, restart_unit, rfc3339_in,
    running_pid, stdout, temp_house, up, StateSub, Supervisor, SILENCE,
};

const FIXTURE: &str = "tests/fixtures/house_arbiter";
const CMD_KEY: &str = "home/cmd/hallway/front_door/locked";
const ARBITER_KEY: &str = "home/arbiter/hallway/front_door/locked";
const EVENT_KEY: &str = "home/health/arbiter/event";
const HOLD_MINUTES_KEY: &str = "home/config/arbiter/hold_minutes";
const HOLDS: &str = "home/hold/arbiter/**";
const HOLD_KEY: &str = "home/hold/arbiter/hallway/front_door/locked";
const SETBACK_HOLD: &str = "home/hold/setback/hallway/front_door/locked";

/// Spawns the fixture and waits for the arbiter to run.
async fn setup() -> (Supervisor, zenoh::Session) {
    up(|| Supervisor::spawn(FIXTURE), &["arbiter"]).await
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

/// The payload of the next sample within the timeout.
async fn next_json(sub: &StateSub, timeout: Duration) -> Option<Value> {
    next_sample(sub, timeout)
        .await
        .map(|msg| msg.value().clone())
}

/// Waits up to 10 s for the hold on `key` to end, its key deleted; `what`
/// names the deletion for the panic.
async fn await_hold_deleted(sub: &StateSub, key: &str, what: &str) {
    await_sample(sub, Duration::from_secs(10), what, |msg| {
        msg.key == key && msg.json.is_none()
    })
    .await;
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
        while let Some(msg) = next_sample(&hold_sub, Duration::from_secs(10)).await {
            let Some(record) = msg.json.filter(|_| msg.key == HOLD_KEY) else {
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
    await_hold_deleted(&hold_sub, HOLD_KEY, "deletion of the hold record at expiry").await;

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
        while let Some(msg) = next_sample(&hold_sub, Duration::from_secs(2)).await {
            if let Some(hold) = msg.json.filter(|h| h["actor"] == json!("probe")) {
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

    // The manual hold counts the automation wish it turned away.
    await_sample(
        &hold_sub,
        Duration::from_secs(10),
        "hold record with refused = 1",
        |msg| msg.json.as_ref().is_some_and(|r| r["refused"] == json!(1)),
    )
    .await;

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
    eventually(
        Duration::from_secs(5),
        "arbitration document naming the applied hold",
        || async {
            let doc = cache_read(&observer, "home/arbitration/arbiter").await?;
            (doc["aspects"][0]["applied"]["id"] == json!("5e7bac02")).then_some(())
        },
    )
    .await;

    restart_unit(&observer, "arbiter").await;
    expect_silence(&arbiter_sub, SILENCE, "forward of the hold already applied").await;

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
    let (mut sup, observer) =
        up(|| Supervisor::spawn_at(&house, &[]), &["arbiter", "curfew"]).await;
    let mut curfew = health_watch(&observer, "curfew").await;
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
    expect_silence(&arbiter_sub, SILENCE, "forward for a holder briefly away").await;

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
    expect_silence(&arbiter_sub, SILENCE, "forward of a delivered hold").await;

    // The adapter reports the forward undelivered, then the device returns.
    observer
        .put(
            "home/health/reflector/event",
            json!({"kind": "drop", "reason": "device-unavailable", "cmd_id": "5e7bac05"})
                .to_string(),
        )
        .await
        .expect("event put");
    // The arbiter hears the drop and the device on separate subscriptions,
    // and a return it hears first sends nothing, as above. The document
    // marks the hold undelivered once the drop is handled.
    eventually(
        Duration::from_secs(10),
        "arbitration document marking the hold undelivered",
        || async {
            let doc = cache_read(&observer, "home/arbitration/arbiter").await?;
            (doc["aspects"][0]["applied"]["undelivered"] == json!(true)).then_some(())
        },
    )
    .await;
    observer
        .put(available, "true")
        .await
        .expect("available put");
    let forwarded = next_json(&arbiter_sub, Duration::from_secs(10))
        .await
        .expect("forward of the undelivered hold");
    assert_eq!(forwarded["id"], json!("5e7bac05"));
    // Sent, the hold is no longer marked.
    eventually(
        Duration::from_secs(10),
        "arbitration document without the undelivered mark",
        || async {
            let doc = cache_read(&observer, "home/arbitration/arbiter").await?;
            let applied = &doc["aspects"][0]["applied"];
            (applied["id"] == json!("5e7bac05") && applied.get("undelivered").is_none())
                .then_some(())
        },
    )
    .await;

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
    let record = await_sample(
        &hold_sub,
        Duration::from_secs(10),
        "family hold record",
        |msg| {
            msg.key == HOLD_KEY
                && msg
                    .json
                    .as_ref()
                    .is_some_and(|r| r["priority"] == json!("manual"))
        },
    )
    .await;
    assert_eq!(
        record.value()["value"],
        json!(false),
        "the hold records what the device did"
    );
    expect_silence(&arbiter_sub, SILENCE, "forward of a report").await;
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
    await_hold_deleted(&hold_sub, HOLD_KEY, "deletion of the hold on a release").await;
    expect_silence(&arbiter_sub, SILENCE, "forward of a release").await;
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
    let held = await_sample(
        &hold_sub,
        Duration::from_secs(10),
        "family hold record",
        |msg| {
            msg.key == HOLD_KEY
                && msg
                    .json
                    .as_ref()
                    .is_some_and(|r| r["priority"] == json!("manual"))
        },
    )
    .await;
    assert_eq!(held.value()["until"], json!(instant));

    // A hold below the manual band is a bug in its publisher.
    let mut bad = envelope(json!(true), "automation", "scheduler");
    bad["hold"] = json!("none");
    put_cmd(&observer, &bad).await;
    // Skip whatever else the steps above reported.
    let dropped = await_sample(&event_sub, Duration::from_secs(10), "drop event", |msg| {
        msg.value()["kind"] == json!("drop")
    })
    .await;
    assert_eq!(dropped.value()["reason"], json!("invalid-command"));

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
    await_sample(
        &hold_sub,
        Duration::from_secs(10),
        "hold taken by the manual wish",
        |msg| msg.key == HOLD_KEY && msg.json.is_some(),
    )
    .await;

    restart_unit(&observer, "arbiter").await;

    put_cmd(
        &observer,
        &envelope(json!(false), "automation", "scheduler"),
    )
    .await;
    let refusal = await_sample(
        &event_sub,
        Duration::from_secs(10),
        "refusal by the restored hold",
        |msg| msg.value()["kind"] == json!("refuse"),
    )
    .await;
    assert_eq!(refusal.value()["holder_actor"], json!("owner"));
    // The restart put nothing back and deleted nothing: the first sample
    // since is the refusal's count on the record that was kept.
    let counted = next_sample(&hold_sub, Duration::from_secs(10))
        .await
        .expect("the refusal is counted on the hold");
    assert_eq!(counted.key, HOLD_KEY);
    assert_eq!(counted.value()["refused"], json!(1));

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
    expect_silence(&event_sub, SILENCE, "event on a free aspect").await;

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
    expect_silence(&arbiter_sub, SILENCE, "forward on release").await;

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
    expect_silence(&event_sub, SILENCE, "event on an independent aspect").await;

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
    expect_silence(&locked_sub, SILENCE, "forward of a refused wish").await;

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
    expect_silence(&arbiter_sub, SILENCE, "forward of a malformed command").await;

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
    edit(
        &house,
        "entities/reflector/front_door.toml",
        "mode = \"arbitrated\"",
        "mode = \"shared\"",
    );
    let (mut sup, observer) = up(|| Supervisor::spawn_at(&house, &[]), &["arbiter"]).await;
    let arbiter_pid = running_pid(&observer, "arbiter").await;
    let arbiter_sub = observer
        .declare_subscriber(ARBITER_KEY)
        .await
        .expect("arbiter subscriber");

    // Shared: the adapter consumes the wish, and the arbiter stays out.
    put_cmd(&observer, &envelope(json!(true), "automation", "scheduler")).await;
    expect_silence(&arbiter_sub, SILENCE, "forward of a shared entity's wish").await;

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
    await_hold_deleted(
        &hold_sub,
        HOLD_KEY,
        "release of the hold once the entity is shared",
    )
    .await;

    sup.shutdown();
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
    edit(
        &house,
        "entities/reflector/front_door.toml",
        "mode = \"arbitrated\"",
        "mode = \"shared\"",
    );
    let (mut sup, observer) = up(|| Supervisor::spawn_at(&house, &[]), &["arbiter"]).await;
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
    let reflector_health = homeostat::bus::health_key("reflector");
    eventually(Duration::from_secs(30), "restarted reflector", || async {
        let health = cache_read(&observer, &reflector_health).await?;
        (health["status"] == json!("running")
            && health["pid"]
                .as_u64()
                .is_some_and(|pid| pid != reflector_pid))
        .then_some(())
    })
    .await;

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
}

/// docs/design.md#family-policy: a family command without a `hold` takes the
/// entity's policy for its value, which reaches the arbiter in the grant
/// table. An unlock holds until the unit hold beneath changes, and a lock
/// holds nothing.
#[tokio::test(flavor = "multi_thread")]
async fn the_family_policy_decides_what_a_tap_holds() {
    let house = temp_house(FIXTURE, "arbiter-policy");
    let entity = house.join("entities/reflector/front_door.toml");
    let mut text = std::fs::read_to_string(&entity).expect("read the entity");
    text.push_str(
        "\n[write_policy.family.locked]\nfalse = { until = \"next change\" }\ntrue = \"none\"\n",
    );
    std::fs::write(&entity, text).expect("write the entity");
    let (mut sup, observer) = up(|| Supervisor::spawn_at(&house, &[]), &["arbiter"]).await;
    let arbiter_sub = observer
        .declare_subscriber(ARBITER_KEY)
        .await
        .expect("arbiter subscriber");
    let hold_sub = observer
        .declare_subscriber(HOLDS)
        .await
        .expect("hold subscriber");
    let setback = |value: bool, id: &str| {
        json!({"value": value, "priority": "automation", "id": id}).to_string()
    };

    observer
        .put(SETBACK_HOLD, setback(true, "5e7bac01"))
        .await
        .expect("hold put");
    let forwarded = next_json(&arbiter_sub, Duration::from_secs(10)).await;
    assert_eq!(forwarded.map(|f| f["id"].clone()), Some(json!("5e7bac01")));

    // An unlock holds over the unit's hold, until it changes.
    let unlock = envelope(json!(false), "manual", "owner");
    put_cmd(&observer, &unlock).await;
    assert_eq!(
        next_json(&arbiter_sub, Duration::from_secs(10)).await,
        Some(unlock)
    );
    let msg = next_sample(&hold_sub, Duration::from_secs(10))
        .await
        .expect("a family hold record");
    assert_eq!(msg.key, HOLD_KEY);
    let record = msg.value();
    assert_eq!(
        (&record["next_change"], &record["beneath"]),
        (&json!(true), &json!("5e7bac01"))
    );
    // Capped at a day: the same time of day, on the next date.
    assert_eq!(hold_seconds(record), 0);
    assert_ne!(
        record["until"].as_str().map(|u| &u[..10]),
        record["since"].as_str().map(|s| &s[..10])
    );

    // The unit changes its mind, so the family's hold ends and its new
    // hold is sent.
    observer
        .put(SETBACK_HOLD, setback(true, "5e7bac02"))
        .await
        .expect("hold put");
    await_hold_deleted(&hold_sub, HOLD_KEY, "end of the family's hold").await;
    let forwarded = next_json(&arbiter_sub, Duration::from_secs(10)).await;
    assert_eq!(forwarded.map(|f| f["id"].clone()), Some(json!("5e7bac02")));

    // A lock holds nothing.
    let lock = envelope(json!(true), "manual", "owner");
    put_cmd(&observer, &lock).await;
    assert_eq!(
        next_json(&arbiter_sub, Duration::from_secs(10)).await,
        Some(lock)
    );
    expect_silence(&hold_sub, SILENCE, "family hold for \"none\"").await;
    sup.shutdown();
}

/// (e) The arbiter honors the unit contract: liveliness token when
/// ready, clean SIGTERM shutdown within the grace, no orphans.
#[tokio::test(flavor = "multi_thread")]
async fn adapter_honors_unit_contract() {
    let (mut sup, observer) = setup().await;
    assert_unit_contract(&mut sup, &observer, "arbiter").await;
}
