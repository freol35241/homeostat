//! Timestamps on the bus and replay after a core restart
//! (docs/design.md#timestamps, docs/design.md#replay-after-a-core-restart).
//!
//! The core's router stamps every sample. The mirror keeps the value with
//! the newest stamp. After a restart the core reads the recorder's newest
//! row of every state series and publishes it again, stamped with when it
//! was recorded, so the house sees its last known state with an honest age
//! instead of nothing.
//!
//! The store outlives the supervisor because it is a file named by
//! `RECORDER_DB`, as in restore.rs.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use zenoh::time::{Timestamp, TimestampId, NTP64};

use crate::common::{
    await_mirror, await_running, await_sample, cache_read, edit, eventually, temp_house, up,
    Scratch, Supervisor,
};

const FIXTURE: &str = "tests/fixtures/house_replay";
const LAMP: &str = "home/state/livingroom/lamp/on";
const FAN: &str = "home/state/bedroom/fan/on";
/// The ID the core's replays carry (`homeostat::supervisor::replay`).
const REPLAY_ID: u8 = 1;

/// Spawns `fixture` against `db` and waits for `units` to run.
async fn setup(fixture: &str, db: &Path, units: &[&str]) -> (Supervisor, zenoh::Session) {
    up(
        || {
            Supervisor::spawn_with_env(
                fixture,
                &[("RECORDER_DB", db.to_str().expect("utf-8 path"))],
            )
        },
        units,
    )
    .await
}

/// Commands `key`'s entity, which the reflector echoes back as state.
async fn command(session: &zenoh::Session, state_key: &str, value: Value) {
    let key = state_key.replacen("home/state/", "home/cmd/", 1);
    let envelope = json!({"value": value, "priority": "manual", "actor": "test"});
    session
        .put(key, envelope.to_string())
        .await
        .expect("command put");
}

/// Every row the recorder holds for an entity's aspect, oldest first.
async fn history(session: &zenoh::Session, entity: &str, aspect: &str) -> Vec<Value> {
    let replies = session
        .get(format!("home/history/state/{entity}/{aspect}?limit=1000"))
        .await
        .expect("history query");
    while let Ok(reply) = replies.recv_async().await {
        if let Ok(sample) = reply.result() {
            let rows: Value =
                serde_json::from_slice(&sample.payload().to_bytes()).expect("rows are JSON");
            return rows.as_array().cloned().unwrap_or_default();
        }
    }
    Vec::new()
}

/// Waits until the recorder's newest row for the aspect is `expected`.
async fn await_recorded(session: &zenoh::Session, entity: &str, aspect: &str, expected: &Value) {
    let what = format!("recorded {expected} for {entity}/{aspect}");
    eventually(Duration::from_secs(30), &what, || async {
        let rows = history(session, entity, aspect).await;
        (rows.last().map(|row| &row["value"]) == Some(expected)).then_some(())
    })
    .await;
}

/// A mirror reply: the value, its stamp and the age the mirror reports.
struct Mirrored {
    value: Value,
    stamp: Option<Timestamp>,
    age_s: f64,
}

async fn mirror_read(session: &zenoh::Session, key: &str) -> Option<Mirrored> {
    let replies = session.get(key).await.expect("mirror get");
    while let Ok(reply) = replies.recv_async().await {
        if let Ok(sample) = reply.result() {
            let age_s = sample
                .attachment()
                .map(|a| {
                    String::from_utf8_lossy(&a.to_bytes())
                        .parse::<f64>()
                        .expect("age is a number")
                })
                .expect("mirror replies carry an age");
            return Some(Mirrored {
                value: serde_json::from_slice(&sample.payload().to_bytes()).expect("JSON"),
                stamp: sample.timestamp().copied(),
                age_s,
            });
        }
    }
    None
}

fn since_epoch() -> Duration {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after the epoch")
}

/// A stamp `ago` in the past, set by a publisher with ID `id`.
fn stamp_ago(ago: Duration, id: u8) -> Timestamp {
    Timestamp::new(
        NTP64::from(since_epoch() - ago),
        TimestampId::try_from(id).expect("non-zero id"),
    )
}

fn stamp_age_s(stamp: &Timestamp) -> f64 {
    (since_epoch().as_secs_f64() - stamp.get_time().to_duration().as_secs_f64()).max(0.0)
}

/// Waits for the probe automation to report a value matching `pred`, and
/// returns the report. The probe repeats its report every half second.
async fn await_seen<F>(session: &zenoh::Session, pred: F) -> Value
where
    F: Fn(&Value) -> bool,
{
    let sub = session
        .declare_subscriber("home/health/probe/event")
        .await
        .expect("probe event subscriber");
    let msg = await_sample(
        &sub,
        Duration::from_mins(1),
        "matching probe report",
        |msg| msg.value()["kind"] == "seen" && pred(msg.value()),
    )
    .await;
    msg.value().clone()
}

/// The core stamps a sample that arrives without a stamp, every subscriber
/// sees that stamp, and the mirror serves it with the value.
#[tokio::test(flavor = "multi_thread")]
async fn the_core_stamps_samples_and_the_mirror_serves_the_stamp() {
    let scratch = Scratch::new("replay-stamps");
    let db = scratch.store();
    let (mut sup, observer) = setup(FIXTURE, &db, &["reflector"]).await;
    let writer = sup.observer().await;
    let key = "home/state/test/probe/level";

    // The core's mirror already matches the key, so a matched publisher
    // would not wait for this subscriber. Publish until it hears one.
    let sub = observer.declare_subscriber(key).await.expect("subscriber");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let sample = loop {
        writer.put(key, "1").await.expect("put");
        if let Ok(sample) = tokio::time::timeout(Duration::from_millis(200), sub.recv_async()).await
        {
            break sample.expect("stream open");
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no sample within 10 s"
        );
    };
    let stamp = *sample.timestamp().expect("the core stamped the sample");
    assert!(
        stamp_age_s(&stamp) < 5.0,
        "stamped on arrival: {stamp_age_s:?}",
        stamp_age_s = stamp_age_s(&stamp)
    );
    assert_ne!(
        stamp.get_id().to_string(),
        writer.zid().to_string(),
        "the router's stamp, not the publisher's"
    );

    await_mirror(&observer, key, &json!(1)).await;
    let mirrored = mirror_read(&observer, key).await.expect("mirrored");
    assert_eq!(mirrored.value, json!(1));
    assert_eq!(
        mirrored.stamp,
        Some(stamp),
        "the mirror serves the sample's own stamp"
    );
    assert!(mirrored.age_s < 5.0, "age {}", mirrored.age_s);

    sup.shutdown();
}

/// An older stamp never replaces a newer value in the mirror, and a value
/// stamped in the past is served with the age its stamp gives it.
#[tokio::test(flavor = "multi_thread")]
async fn the_mirror_keeps_the_newest_stamp_and_ages_a_past_one() {
    let scratch = Scratch::new("replay-newest");
    let db = scratch.store();
    let (mut sup, observer) = setup(FIXTURE, &db, &["reflector"]).await;
    let writer = sup.observer().await;
    let ten_minutes = Duration::from_mins(10);

    // A live value, then an older one for the same key: the live one stays.
    let held = "home/state/test/probe/held";
    writer.put(held, "1").await.expect("live put");
    await_mirror(&observer, held, &json!(1)).await;
    writer
        .put(held, "2")
        .timestamp(stamp_ago(ten_minutes, 7))
        .await
        .expect("old put");
    // The mirror takes one session's puts in order, so once a later put
    // from `writer` is mirrored, the old one has been handled.
    let marker = "home/state/test/probe/marker";
    writer.put(marker, "1").await.expect("marker put");
    await_mirror(&observer, marker, &json!(1)).await;
    assert_eq!(cache_read(&observer, held).await, Some(json!(1)));

    // A value stamped ten minutes ago on an empty key: kept, and ten
    // minutes old.
    let past = "home/state/test/probe/past";
    let stamp = stamp_ago(ten_minutes, 7);
    writer
        .put(past, "3")
        .timestamp(stamp)
        .await
        .expect("past put");
    await_mirror(&observer, past, &json!(3)).await;
    let mirrored = mirror_read(&observer, past).await.expect("mirrored");
    assert_eq!(mirrored.value, json!(3));
    assert_eq!(mirrored.stamp, Some(stamp));
    assert!(
        (mirrored.age_s - 600.0).abs() < 5.0,
        "age from the stamp: {}",
        mirrored.age_s
    );

    // A live value after it replaces it, and is fresh.
    writer.put(past, "4").await.expect("live put");
    await_mirror(&observer, past, &json!(4)).await;
    let mirrored = mirror_read(&observer, past).await.expect("mirrored");
    assert!(mirrored.age_s < 5.0, "age {}", mirrored.age_s);

    sup.shutdown();
}

/// The scenario #204 is about. A value is recorded, the core goes down and
/// comes back on the same store, and the value is back on the bus with the
/// age it really has: in the mirror, to a unit's `subscribe`, and without a
/// second row in the store. A live value afterwards replaces it.
#[tokio::test(flavor = "multi_thread")]
async fn state_comes_back_after_a_core_restart_with_its_age() {
    let scratch = Scratch::new("replay-restart");
    let db = scratch.store();
    let units = ["recorder", "reflector", "probe"];
    let (mut sup, observer) = setup(FIXTURE, &db, &units).await;

    command(&observer, LAMP, json!(true)).await;
    await_mirror(&observer, LAMP, &json!(true)).await;
    await_recorded(&observer, "lamp", "on", &json!(true)).await;

    // Down for a few seconds, so the replayed value is measurably old.
    sup.shutdown();
    drop(observer);
    let gap = Duration::from_secs(3);
    tokio::time::sleep(gap).await;
    let (mut sup, observer) = setup(FIXTURE, &db, &units).await;

    // In the mirror, aged from when it was recorded, under the replay ID.
    await_mirror(&observer, LAMP, &json!(true)).await;
    let mirrored = mirror_read(&observer, LAMP).await.expect("replayed");
    assert_eq!(mirrored.value, json!(true));
    assert!(
        mirrored.age_s >= gap.as_secs_f64() && mirrored.age_s < 120.0,
        "age {}",
        mirrored.age_s
    );
    let stamp = mirrored.stamp.expect("a replay is stamped");
    assert_eq!(
        *stamp.get_id(),
        TimestampId::try_from(REPLAY_ID).expect("id"),
        "the replay ID"
    );

    // To a unit, through `subscribe`, with the same age.
    let seen = await_seen(&observer, |e| e["value"] == json!(true)).await;
    let age = seen["age_s"].as_f64().expect("age_s");
    assert!(age >= gap.as_secs_f64(), "probe saw age {age}");

    // Nothing the store did not hold.
    assert_eq!(
        cache_read(&observer, FAN).await,
        None,
        "the fan was never recorded"
    );

    // A live value replaces the replay everywhere.
    command(&observer, LAMP, json!(false)).await;
    await_mirror(&observer, LAMP, &json!(false)).await;
    let mirrored = mirror_read(&observer, LAMP).await.expect("live");
    assert!(mirrored.age_s < 5.0, "age {}", mirrored.age_s);
    let seen = await_seen(&observer, |e| e["value"] == json!(false)).await;
    assert!(seen["age_s"].as_f64().expect("age_s") < 5.0, "{seen}");

    // No second row for what the store held. The probe saw the replay
    // before the live command went out, so the router sent the recorder
    // the replay ahead of the live value. Once the live value is recorded,
    // a recorded replay would be too.
    await_recorded(&observer, "lamp", "on", &json!(false)).await;
    let rows = history(&observer, "lamp", "on").await;
    let values: Vec<&Value> = rows.iter().map(|row| &row["value"]).collect();
    assert_eq!(
        values,
        [&json!(true), &json!(false)],
        "the replay was not recorded again: {rows:?}"
    );

    sup.shutdown();
}

/// A value published after the restart, before the recorder is up, is not
/// overwritten by the replay of an older one, whichever reaches the mirror
/// first.
#[tokio::test(flavor = "multi_thread")]
async fn a_live_value_is_not_overwritten_by_the_replay() {
    let scratch = Scratch::new("replay-live-wins");
    let db = scratch.store();
    let units = ["recorder", "reflector", "probe"];
    let (mut sup, observer) = setup(FIXTURE, &db, &units).await;
    command(&observer, LAMP, json!(true)).await;
    await_recorded(&observer, "lamp", "on", &json!(true)).await;
    sup.shutdown();
    drop(observer);

    let (mut sup, observer) = setup(FIXTURE, &db, &["reflector"]).await;
    command(&observer, LAMP, json!(false)).await;
    await_mirror(&observer, LAMP, &json!(false)).await;
    for unit in ["recorder", "probe"] {
        await_running(&observer, unit, Duration::from_mins(2)).await;
    }
    // Long enough for the replay to have been published, had it been.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(cache_read(&observer, LAMP).await, Some(json!(false)));
    let seen = await_seen(&observer, |_| true).await;
    assert_eq!(
        seen["value"],
        json!(false),
        "the probe's last value: {seen}"
    );

    sup.shutdown();
}

/// An entity that moved room while the core was down is not replayed under
/// its old key, and not under its new one either: the value was recorded
/// somewhere it no longer is.
#[tokio::test(flavor = "multi_thread")]
async fn a_moved_entity_is_not_replayed() {
    let scratch = Scratch::new("replay-moved");
    let db = scratch.store();
    let (mut sup, observer) = setup(FIXTURE, &db, &["recorder", "reflector"]).await;
    command(&observer, LAMP, json!(true)).await;
    command(&observer, FAN, json!(true)).await;
    await_recorded(&observer, "lamp", "on", &json!(true)).await;
    await_recorded(&observer, "fan", "on", &json!(true)).await;
    sup.shutdown();
    drop(observer);

    // The same store, a house where the lamp is in the kitchen.
    let house = temp_house(FIXTURE, "replay-moved");
    let lamp = "entities/reflector/lamp.toml";
    edit(
        &house,
        lamp,
        r#"room = "livingroom""#,
        r#"room = "kitchen""#,
    );
    edit(
        &house,
        lamp,
        r#"en = "living room lamp""#,
        r#"en = "kitchen lamp""#,
    );
    for file in ["probe.py", "probe.py.lock", "probe.toml"] {
        std::fs::remove_file(house.join("units").join(file)).expect("remove probe file");
    }
    let (mut sup, observer) = up(
        || Supervisor::spawn_at(&house, &[("RECORDER_DB", db.to_str().expect("utf-8 path"))]),
        &["recorder", "reflector"],
    )
    .await;
    // The fan did not move, so its replay shows the replay has run.
    await_mirror(&observer, FAN, &json!(true)).await;
    assert_eq!(
        cache_read(&observer, LAMP).await,
        None,
        "not under the old room"
    );
    assert_eq!(
        cache_read(&observer, "home/state/kitchen/lamp/on").await,
        None,
        "not under the new room either"
    );

    sup.shutdown();
}

const HOLDS_FIXTURE: &str = "tests/fixtures/house_replay_holds";
const FAMILY_HOLD: &str = "home/hold/arbiter/hallway/front_door/locked";
const LOCK_CMD: &str = "home/cmd/hallway/front_door/locked";

/// Waits until the recorder holds a record at `key`, read from
/// home/history/holds.
async fn await_standing(session: &zenoh::Session, key: &str) -> Value {
    let what = format!("recorded hold at {key}");
    eventually(Duration::from_secs(20), &what, || async {
        let rows = cache_read(session, "home/history/holds").await?;
        let row = rows.as_array()?.iter().find(|r| r["key"] == json!(key))?;
        Some(row["value"].clone())
    })
    .await
}

/// The family's hold outlives a core restart
/// (docs/design.md#replay-after-a-core-restart): the recorder keeps it and
/// the arbitration document, the core gives both back once the recorder
/// runs and says the replay is done, and the restarted arbiter, which waits
/// for that, goes on refusing what the hold refused.
#[tokio::test(flavor = "multi_thread")]
async fn the_familys_hold_comes_back_after_a_core_restart() {
    let scratch = Scratch::new("replay-holds");
    let db = scratch.store();
    let units = ["recorder", "reflector", "arbiter"];
    let (mut sup, observer) = setup(HOLDS_FIXTURE, &db, &units).await;
    observer
        .put(
            LOCK_CMD,
            json!({"value": true, "priority": "manual", "actor": "owner", "id": "fa111111"})
                .to_string(),
        )
        .await
        .expect("cmd put");
    let kept = await_standing(&observer, FAMILY_HOLD).await;
    assert_eq!(kept["actor"], json!("owner"));
    await_standing(&observer, "home/arbitration/arbiter").await;

    sup.shutdown();
    drop(observer);
    let (mut sup, observer) = setup(HOLDS_FIXTURE, &db, &units).await;

    eventually(
        Duration::from_secs(20),
        "replay marker \"done\"",
        || async {
            (cache_read(&observer, "home/meta/system/replay").await == Some(json!("done")))
                .then_some(())
        },
    )
    .await;
    let back = cache_read(&observer, FAMILY_HOLD)
        .await
        .expect("the family's hold is back in the mirror");
    assert_eq!(
        (back["actor"].clone(), back["id"].clone()),
        (json!("owner"), json!("fa111111"))
    );

    let events = observer
        .declare_subscriber("home/health/arbiter/event")
        .await
        .expect("event subscriber");
    observer
        .put(
            LOCK_CMD,
            json!({"value": false, "priority": "automation", "actor": "scheduler", "id": "5c4ed001"})
                .to_string(),
        )
        .await
        .expect("cmd put");
    let refused = await_sample(
        &events,
        Duration::from_secs(10),
        "refusal below the family's hold",
        |msg| msg.value()["kind"] == json!("refuse"),
    )
    .await;
    assert_eq!(refused.value()["holder_actor"], json!("owner"));

    sup.shutdown();
}
