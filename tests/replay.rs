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

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use homeostat::bus::HealthStatus;
use serde_json::{json, Value};
use zenoh::time::{Timestamp, TimestampId, NTP64};

use common::{await_health, await_mirror, cache_read, health_watch, Supervisor};

const FIXTURE: &str = "tests/fixtures/house_replay";
const MOVED: &str = "tests/fixtures/house_replay_moved";
const LAMP: &str = "home/state/livingroom/lamp/on";
const FAN: &str = "home/state/bedroom/fan/on";
/// The ID the core's replays carry (`homeostat::supervisor::replay`).
const REPLAY_ID: u8 = 1;

fn store_path(test: &str) -> PathBuf {
    let path =
        std::env::temp_dir().join(format!("homeostat-replay-{test}-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    path
}

/// Spawns `fixture` against `db` and waits for `units` to run.
async fn setup(fixture: &str, db: &Path, units: &[&str]) -> (Supervisor, zenoh::Session) {
    let sup = Supervisor::spawn_with_env(
        fixture,
        &[("RECORDER_DB", db.to_str().expect("utf-8 path"))],
    );
    let observer = sup.observer().await;
    for unit in units {
        let mut health = health_watch(&observer, unit).await;
        await_health(&mut health, Duration::from_mins(2), |h| {
            h.status == HealthStatus::Running
        })
        .await;
    }
    (sup, observer)
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
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if history(session, entity, aspect)
            .await
            .last()
            .map(|row| &row["value"])
            == Some(expected)
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "recorder never held {expected} for {entity}/{aspect}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
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
    let deadline = tokio::time::Instant::now() + Duration::from_mins(1);
    let mut last = Value::Null;
    loop {
        let sample = tokio::time::timeout_at(deadline, sub.recv_async())
            .await
            .unwrap_or_else(|_| panic!("probe never reported a match; last: {last}"))
            .expect("event stream open");
        let event: Value = serde_json::from_slice(&sample.payload().to_bytes()).expect("JSON");
        if event["kind"] == "seen" && pred(&event) {
            return event;
        }
        last = event;
    }
}

/// The core stamps a sample that arrives without a stamp, every subscriber
/// sees that stamp, and the mirror serves it with the value.
#[tokio::test(flavor = "multi_thread")]
async fn the_core_stamps_samples_and_the_mirror_serves_the_stamp() {
    let db = store_path("stamps");
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
    let _ = std::fs::remove_file(&db);
}

/// An older stamp never replaces a newer value in the mirror, and a value
/// stamped in the past is served with the age its stamp gives it.
#[tokio::test(flavor = "multi_thread")]
async fn the_mirror_keeps_the_newest_stamp_and_ages_a_past_one() {
    let db = store_path("newest");
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
    tokio::time::sleep(Duration::from_millis(500)).await;
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
    let _ = std::fs::remove_file(&db);
}

/// The scenario #204 is about. A value is recorded, the core goes down and
/// comes back on the same store, and the value is back on the bus with the
/// age it really has: in the mirror, to a unit's `subscribe`, and without a
/// second row in the store. A live value afterwards replaces it.
#[tokio::test(flavor = "multi_thread")]
async fn state_comes_back_after_a_core_restart_with_its_age() {
    let db = store_path("restart");
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

    // Nothing the store did not hold, and no second row for what it did.
    assert_eq!(
        cache_read(&observer, FAN).await,
        None,
        "the fan was never recorded"
    );
    tokio::time::sleep(Duration::from_secs(2)).await;
    let rows = history(&observer, "lamp", "on").await;
    assert_eq!(rows.len(), 1, "the replay was not recorded again: {rows:?}");

    // A live value replaces the replay everywhere.
    command(&observer, LAMP, json!(false)).await;
    await_mirror(&observer, LAMP, &json!(false)).await;
    let mirrored = mirror_read(&observer, LAMP).await.expect("live");
    assert!(mirrored.age_s < 5.0, "age {}", mirrored.age_s);
    let seen = await_seen(&observer, |e| e["value"] == json!(false)).await;
    assert!(seen["age_s"].as_f64().expect("age_s") < 5.0, "{seen}");

    sup.shutdown();
    let _ = std::fs::remove_file(&db);
}

/// A value published after the restart, before the recorder is up, is not
/// overwritten by the replay of an older one, whichever reaches the mirror
/// first.
#[tokio::test(flavor = "multi_thread")]
async fn a_live_value_is_not_overwritten_by_the_replay() {
    let db = store_path("live-wins");
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
        let mut health = health_watch(&observer, unit).await;
        await_health(&mut health, Duration::from_mins(2), |h| {
            h.status == HealthStatus::Running
        })
        .await;
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
    let _ = std::fs::remove_file(&db);
}

/// An entity that moved room while the core was down is not replayed under
/// its old key, and not under its new one either: the value was recorded
/// somewhere it no longer is.
#[tokio::test(flavor = "multi_thread")]
async fn a_moved_entity_is_not_replayed() {
    let db = store_path("moved");
    let (mut sup, observer) = setup(FIXTURE, &db, &["recorder", "reflector"]).await;
    command(&observer, LAMP, json!(true)).await;
    command(&observer, FAN, json!(true)).await;
    await_recorded(&observer, "lamp", "on", &json!(true)).await;
    await_recorded(&observer, "fan", "on", &json!(true)).await;
    sup.shutdown();
    drop(observer);

    // The same store, a house where the lamp is in the kitchen.
    let (mut sup, observer) = setup(MOVED, &db, &["recorder", "reflector"]).await;
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
    let _ = std::fs::remove_file(&db);
}
