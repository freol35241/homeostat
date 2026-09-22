//! Step-5a integration tests: the recorder, end to end, on a real
//! supervisor running the step-4 units (clock, evening_lights, reflector)
//! plus the recorder. Assertions run against both the bus and the store
//! (rusqlite opens the same SQLite file the recorder writes).
//!
//! Each test gets its own store via the RECORDER_DB environment variable,
//! expanded by the recorder from its manifest's [discovery] endpoint —
//! no fixed paths, like no fixed ports.
//!
//! The tests publish state under rooms outside the `downstairs` zone
//! (attic, cellar), so the evening_lights automation running on the real
//! clock never reacts to them.

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use homeostat::bus::HealthStatus;
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};
use zenoh::handlers::FifoChannelHandler;
use zenoh::pubsub::Subscriber;
use zenoh::sample::Sample;

use common::{
    await_health, await_mirror, config_write, health_watch, matched_publisher, Publisher,
    Supervisor,
};

const FIXTURE: &str = "tests/fixture_house_recorder";

type Sub = Subscriber<FifoChannelHandler<Sample>>;

/// A per-test store path: unique like the harness's per-test bus port.
fn store_path(test: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "homeostat-recorder-{test}-{}.db",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    path
}

/// Spawns the fixture with its store at `db` and waits for the recorder.
async fn setup(db: &Path) -> (Supervisor, zenoh::Session) {
    let sup = Supervisor::spawn_with_env(
        FIXTURE,
        &[("RECORDER_DB", db.to_str().expect("utf-8 path"))],
    );
    let observer = sup.observer().await;
    let mut recorder = health_watch(&observer, "recorder").await;
    await_health(&mut recorder, Duration::from_secs(60), |h| {
        h.status == HealthStatus::Running
    })
    .await;
    (sup, observer)
}

async fn put(publisher: &Publisher, value: Value) {
    publisher.put(value.to_string()).await.expect("state put");
}

/// Rows for `sql` against the store, empty while the store isn't there yet.
fn read_rows(db: &Path, sql: &str) -> Vec<Vec<SqlValue>> {
    let Ok(conn) = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return Vec::new();
    };
    let _ = conn.busy_timeout(Duration::from_secs(2));
    let Ok(mut stmt) = conn.prepare(sql) else {
        return Vec::new();
    };
    let cols = stmt.column_count();
    let rows = stmt.query_map([], |row| {
        (0..cols).map(|i| row.get::<_, SqlValue>(i)).collect()
    });
    match rows {
        Ok(rows) => rows.filter_map(Result::ok).collect(),
        Err(_) => Vec::new(),
    }
}

/// The per-series tally on `series` must say exactly what an aggregate
/// over `samples` says. It is maintained incrementally — by a trigger on
/// insert, by `_purge` on delete — so drift is the one failure mode a
/// denormalized count has, and every test that writes or purges checks
/// for it here rather than trusting the read that now depends on it.
fn assert_tally_matches_samples(db: &Path) {
    let tally = read_rows(
        db,
        "SELECT id, row_count, oldest_ts, newest_ts FROM series ORDER BY id",
    );
    let truth = read_rows(
        db,
        "SELECT series.id, COUNT(samples.series_id), MIN(ts), MAX(ts) FROM series
         LEFT JOIN samples ON samples.series_id = series.id GROUP BY series.id ORDER BY series.id",
    );
    assert_eq!(tally, truth, "series tally drifted from samples");
}

/// Polls the store until `sql` yields at least `n` rows; panics on timeout.
async fn rows_eventually(db: &Path, sql: &str, n: usize, timeout: Duration) -> Vec<Vec<SqlValue>> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let rows = read_rows(db, sql);
        if rows.len() >= n {
            return rows;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{sql}: {} rows, wanted {n}, within {timeout:?}",
            rows.len()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Reads recorder health events until one satisfies `pred`.
async fn await_event<F>(sub: &Sub, timeout: Duration, pred: F) -> Value
where
    F: Fn(&Value) -> bool,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let sample = tokio::time::timeout_at(deadline, sub.recv_async())
            .await
            .expect("recorder event within timeout")
            .expect("event stream open");
        let event: Value =
            serde_json::from_slice(&sample.payload().to_bytes()).expect("event is JSON");
        if pred(&event) {
            return event;
        }
    }
}

/// Ok replies for a history get, as (concrete key, decoded rows).
async fn history_get(session: &zenoh::Session, selector: &str) -> Vec<(String, Value)> {
    let replies = session.get(selector).await.expect("history query");
    let mut out = Vec::new();
    while let Ok(reply) = replies.recv_async().await {
        let sample = reply.result().unwrap_or_else(|err| {
            panic!(
                "history reply error: {}",
                String::from_utf8_lossy(&err.payload().to_bytes())
            )
        });
        out.push((
            sample.key_expr().to_string(),
            serde_json::from_slice(&sample.payload().to_bytes()).expect("reply is JSON"),
        ));
    }
    out
}

fn now_us() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("epoch")
        .as_micros() as i64
}

/// (a) State published on the bus lands in the store with entity, room,
/// aspect, and correctly typed value; commands and accepted config edits
/// land too (the audit trail); non-scalar payloads leave a health event
/// and never a row.
#[tokio::test(flavor = "multi_thread")]
async fn state_lands_typed_in_store() {
    let db = store_path("typed");
    let (mut sup, observer) = setup(&db).await;

    let on = matched_publisher(&observer, "home/state/attic/probe/on").await;
    let temperature = matched_publisher(&observer, "home/state/attic/probe/temperature").await;
    let mode = matched_publisher(&observer, "home/state/attic/probe/mode").await;
    put(&on, json!(true)).await;
    put(&temperature, json!(21.5)).await;
    put(&mode, json!("eco")).await;

    let rows = rows_eventually(
        &db,
        "SELECT class, room, aspect, kind, value FROM history \
         WHERE entity = 'probe' ORDER BY aspect",
        3,
        Duration::from_secs(20),
    )
    .await;
    let expect = |aspect: &str, kind: &str, value: SqlValue| {
        vec![
            SqlValue::Text("state".into()),
            SqlValue::Text("attic".into()),
            SqlValue::Text(aspect.into()),
            SqlValue::Text(kind.into()),
            value,
        ]
    };
    assert_eq!(
        rows,
        vec![
            expect("mode", "string", SqlValue::Text("eco".into())),
            expect("on", "bool", SqlValue::Integer(1)),
            expect("temperature", "number", SqlValue::Real(21.5)),
        ]
    );

    // A health event subscriber, reused below for the invalid-command and
    // non-scalar drop scenarios.
    let events = observer
        .declare_subscriber("home/health/recorder/event")
        .await
        .expect("event subscriber");

    // Commands are recorded in the same table under class 'cmd' — the
    // envelope's value unwrapped into samples, the full envelope into
    // events: the "who" audit design.md anticipated, priority and actor
    // now travel with every command.
    let cmd = matched_publisher(&observer, "home/cmd/attic/probe/on").await;
    put(
        &cmd,
        json!({"value": false, "priority": "automation", "actor": "test"}),
    )
    .await;
    let rows = rows_eventually(
        &db,
        "SELECT kind, value FROM history WHERE class = 'cmd' AND entity = 'probe'",
        1,
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(
        rows,
        vec![vec![SqlValue::Text("bool".into()), SqlValue::Integer(0),]]
    );
    let event_rows = rows_eventually(
        &db,
        "SELECT payload FROM events WHERE key = 'home/cmd/attic/probe/on'",
        1,
        Duration::from_secs(10),
    )
    .await;
    let SqlValue::Text(envelope_text) = &event_rows[0][0] else {
        panic!("event payload is not text: {:?}", event_rows[0][0]);
    };
    let envelope: Value = serde_json::from_str(envelope_text).expect("event payload is JSON");
    assert_eq!(
        envelope,
        json!({"value": false, "priority": "automation", "actor": "test"}),
        "the full envelope lands in events, not just the unwrapped value"
    );

    // An envelope-less cmd payload (the pre-envelope bare-value shape) is
    // invalid traffic: dropped with a health event, never a samples row.
    let bad_cmd = matched_publisher(&observer, "home/cmd/attic/probe/brightness").await;
    put(&bad_cmd, json!(42)).await;
    let event = await_event(&events, Duration::from_secs(10), |e| {
        e["kind"] == "drop" && e["key"] == "home/cmd/attic/probe/brightness"
    })
    .await;
    assert_eq!(event["reason"], json!("invalid-command"));
    assert!(
        read_rows(&db, "SELECT * FROM history WHERE aspect = 'brightness'").is_empty(),
        "envelope-less cmd payload became a row"
    );

    // An accepted config edit lands in the events audit table (rejects
    // never reach the bus, so they can't land — pinned in step 4).
    config_write(
        &observer,
        "home/config/evening_lights/off_time",
        json!("21:30"),
    )
    .await
    .expect("config write accepted");
    let rows = rows_eventually(
        &db,
        "SELECT payload FROM events \
         WHERE key = 'home/config/evening_lights/off_time'",
        1,
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(rows[0], vec![SqlValue::Text("\"21:30\"".into())]);

    // Supervisor health transitions land there too.
    rows_eventually(
        &db,
        "SELECT key FROM events WHERE key LIKE 'home/health/%'",
        1,
        Duration::from_secs(10),
    )
    .await;

    // A non-scalar payload is dropped with a health event, never a row.
    let color = matched_publisher(&observer, "home/state/attic/probe/color").await;
    put(&color, json!({"r": 255, "g": 0, "b": 0})).await;
    let event = await_event(&events, Duration::from_secs(10), |e| e["kind"] == "drop").await;
    assert_eq!(event["reason"], json!("non-scalar"));
    assert_eq!(event["key"], json!("home/state/attic/probe/color"));
    assert!(
        read_rows(&db, "SELECT * FROM history WHERE aspect = 'color'").is_empty(),
        "non-scalar payload became a row"
    );

    // H4, defense in depth: a raw NaN on the bus (serde_json::Value can't
    // hold it, so this publishes the literal bytes directly — the shape a
    // publisher that skips the SDK's put_json guard, or a future one,
    // could still produce) is dropped as "non-finite", never a row — and
    // the writer keeps working afterward rather than mistaking a refused
    // row for a dead backend and stalling on it.
    let gauge = matched_publisher(&observer, "home/state/attic/probe/gauge").await;
    gauge.put("NaN").await.expect("raw NaN put");
    let event = await_event(&events, Duration::from_secs(10), |e| e["kind"] == "drop").await;
    assert_eq!(event["reason"], json!("non-finite"));
    assert_eq!(event["key"], json!("home/state/attic/probe/gauge"));
    assert!(
        read_rows(&db, "SELECT * FROM history WHERE aspect = 'gauge'").is_empty(),
        "a non-finite payload became a row"
    );
    put(&gauge, json!(3.0)).await;
    rows_eventually(
        &db,
        "SELECT value FROM history WHERE aspect = 'gauge'",
        1,
        Duration::from_secs(10),
    )
    .await;

    sup.shutdown();
}

/// (b) The same entity publishing under a new room continues ONE series
/// with a tag transition — what an entity move looks like from the bus.
#[tokio::test(flavor = "multi_thread")]
async fn entity_move_is_a_tag_transition() {
    let db = store_path("move");
    let (mut sup, observer) = setup(&db).await;

    let attic = matched_publisher(&observer, "home/state/attic/rover/on").await;
    put(&attic, json!(true)).await;
    rows_eventually(
        &db,
        "SELECT room FROM history WHERE entity = 'rover'",
        1,
        Duration::from_secs(20),
    )
    .await;

    // The move: same entity, new room.
    let cellar = matched_publisher(&observer, "home/state/cellar/rover/on").await;
    put(&cellar, json!(false)).await;

    // In the store: one series identity, the room tag transitions.
    let rows = rows_eventually(
        &db,
        "SELECT class, entity, aspect, room, value FROM history \
         WHERE entity = 'rover' ORDER BY ts",
        2,
        Duration::from_secs(10),
    )
    .await;
    let series = |room: &str, value: i64| {
        vec![
            SqlValue::Text("state".into()),
            SqlValue::Text("rover".into()),
            SqlValue::Text("on".into()),
            SqlValue::Text(room.into()),
            SqlValue::Integer(value),
        ]
    };
    assert_eq!(rows, vec![series("attic", 1), series("cellar", 0)]);

    // Over the bus: one reply — one series, never two.
    let replies = history_get(&observer, "home/history/state/rover/on").await;
    assert_eq!(replies.len(), 1, "a move must not split the series");
    let (key, rows) = &replies[0];
    assert_eq!(key, "home/history/state/rover/on");
    let rooms: Vec<&str> = rows
        .as_array()
        .expect("reply is an array")
        .iter()
        .map(|r| r["room"].as_str().expect("room is a string"))
        .collect();
    assert_eq!(rooms, vec!["attic", "cellar"], "the tag transition");

    sup.shutdown();
}

/// (c) The backend-outage policy is observable: make the store unwritable
/// (what "the backend is down" means for an embedded engine), publish
/// state, restore it — samples buffer with their receive-time timestamps,
/// health events mark the outage and the recovery, nothing is lost.
#[tokio::test(flavor = "multi_thread")]
async fn backend_outage_buffers_and_flushes() {
    let db = store_path("outage");
    let (mut sup, observer) = setup(&db).await;
    let events = observer
        .declare_subscriber("home/health/recorder/event")
        .await
        .expect("event subscriber");

    let gauge = matched_publisher(&observer, "home/state/attic/gauge/level").await;
    put(&gauge, json!(1)).await;
    rows_eventually(
        &db,
        "SELECT value FROM history WHERE entity = 'gauge'",
        1,
        Duration::from_secs(20),
    )
    .await;

    // Kill the backend: the store file becomes unwritable.
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o444))
        .expect("chmod store read-only");

    let before_put = now_us();
    put(&gauge, json!(2)).await;
    await_event(&events, Duration::from_secs(30), |e| {
        e["kind"] == "backend-outage"
    })
    .await;
    let after_outage = now_us();

    // Nothing landed while down (the store is still readable).
    assert_eq!(
        read_rows(&db, "SELECT value FROM history WHERE entity = 'gauge'").len(),
        1,
        "sample leaked into an unwritable store"
    );

    // More state during the outage joins the buffer; only one outage
    // event marks the whole transition.
    put(&gauge, json!(3)).await;

    // Restore the backend.
    std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o644))
        .expect("chmod store writable");
    let restored = await_event(&events, Duration::from_secs(30), |e| {
        e["kind"] == "backend-restored"
    })
    .await;
    assert!(
        restored["flushed"].as_i64().expect("flushed count") >= 2,
        "restored event reports the flush: {restored}"
    );
    assert_eq!(
        restored["dropped"],
        json!(0),
        "nothing overflowed: {restored}"
    );

    // The buffer flushed in order, and the buffered samples carry their
    // receive-time timestamps — the outage is invisible in the data.
    let rows = rows_eventually(
        &db,
        "SELECT value, ts FROM history WHERE entity = 'gauge' ORDER BY ts",
        3,
        Duration::from_secs(10),
    )
    .await;
    let values: Vec<&SqlValue> = rows.iter().map(|r| &r[0]).collect();
    assert_eq!(
        values,
        vec![
            &SqlValue::Integer(1),
            &SqlValue::Integer(2),
            &SqlValue::Integer(3)
        ]
    );
    let SqlValue::Integer(ts2) = rows[1][1] else {
        panic!("ts is an integer");
    };
    assert!(
        ts2 >= before_put && ts2 <= after_outage,
        "buffered sample keeps its receive time: {ts2} not in [{before_put}, {after_outage}]"
    );

    sup.shutdown();
}

/// The store is legible over the bus: home/history/stats replies one
/// message with the file's size, one aggregate per series keyed by its
/// history key, and the events table's count and bounds — what choosing a
/// retention window needs, on a host that may have no sqlite3 binary.
#[tokio::test(flavor = "multi_thread")]
async fn stats_describe_the_store() {
    let db = store_path("stats");
    let (mut sup, observer) = setup(&db).await;

    let before = now_us();
    let power = matched_publisher(&observer, "home/state/attic/meter/power").await;
    for value in [1.5, 2.5, 3.5] {
        put(&power, json!(value)).await;
    }
    let level = matched_publisher(&observer, "home/state/attic/gauge/level").await;
    put(&level, json!(7)).await;
    rows_eventually(&db, "SELECT value FROM samples", 4, Duration::from_secs(20)).await;
    let after = now_us();

    let replies = history_get(&observer, "home/history/stats").await;
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].0, "home/history/stats");
    let stats = &replies[0].1;
    assert_eq!(stats["store_version"], json!(5));
    let file_bytes = stats["file_bytes"].as_i64().expect("file size");
    assert!(file_bytes >= 4096, "page_count * page_size: {stats}");
    assert!(stats["freelist_bytes"].as_i64().expect("freelist") >= 0);

    let series = stats["series"].as_object().expect("series map");
    assert_eq!(series.len(), 2, "one entry per series: {stats}");
    let power = &series["home/history/state/meter/power"];
    assert_eq!(power["rows"], json!(3));
    assert_eq!(series["home/history/state/gauge/level"]["rows"], json!(1));
    let oldest = power["oldest"].as_str().expect("RFC3339 oldest");
    let newest = power["newest"].as_str().expect("RFC3339 newest");
    assert!(oldest.ends_with("+00:00") && oldest <= newest, "{power}");

    // Those aggregates are read off `series`, not computed over the rows:
    // the reply is only as true as the tally behind it.
    assert_tally_matches_samples(&db);

    // The rate the owner reads to see which series is filling the file.
    // Three puts microseconds apart make it enormous but finite; the
    // single-row series has no span to divide by and says so.
    assert!(
        power["rows_per_day"].as_f64().expect("a rate") > 0.0,
        "{power}"
    );
    assert_eq!(
        series["home/history/state/gauge/level"]["rows_per_day"],
        json!(null),
        "one row is not a rate: {stats}"
    );

    // Events: the fixture's own health transitions are already there,
    // stamped in the recorder's µs convention.
    let events = &stats["events"];
    assert!(
        events["rows"].as_i64().expect("events count") >= 1,
        "{events}"
    );
    let newest = events["newest"].as_i64().expect("µs newest");
    assert!(newest <= after && events["oldest"].as_i64().expect("µs oldest") <= newest);
    assert!(
        newest >= before - 120_000_000,
        "events bounds are recent: {events}"
    );

    // A wildcard over history fans out over series and never includes the
    // stats reply, so a samples reader never sees a foreign payload.
    let replies = history_get(&observer, "home/history/**").await;
    assert_eq!(replies.len(), 2, "series only: {replies:?}");
    assert!(replies
        .iter()
        .all(|(key, _)| key.starts_with("home/history/state/")));

    sup.shutdown();
}

/// Retention: rows older than a table's window are purged and the pages
/// returned to the file, one `purge` health event per purge that deleted
/// anything. The two windows are separate: samples go, the audit trail
/// stays at its own (unset) window. A window change applies at once.
#[tokio::test(flavor = "multi_thread")]
async fn retention_purges_old_rows() {
    let db = store_path("retention");
    let (mut sup, observer) = setup(&db).await;
    let events = observer
        .declare_subscriber("home/health/recorder/event")
        .await
        .expect("event subscriber");

    let power = matched_publisher(&observer, "home/state/attic/meter/power").await;
    for value in [1.5, 2.5, 3.5] {
        put(&power, json!(value)).await;
    }
    let level = matched_publisher(&observer, "home/state/attic/gauge/level").await;
    put(&level, json!(7)).await;
    rows_eventually(&db, "SELECT value FROM samples", 4, Duration::from_secs(20)).await;
    let audit_before = read_rows(&db, "SELECT ts FROM events").len();
    assert!(
        audit_before >= 1,
        "health transitions are in the audit trail"
    );

    // Everything recorded so far ages past a window of ~0.86 s; setting
    // the window is what triggers the purge.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    config_write(
        &observer,
        "home/config/recorder/retain_samples_days",
        json!(1e-5),
    )
    .await
    .expect("in-constraint write accepted");
    let purge = await_event(&events, Duration::from_secs(30), |e| e["kind"] == "purge").await;
    assert_eq!(purge["samples"], json!(4), "both series purged: {purge}");
    assert_eq!(
        purge["events"],
        json!(0),
        "events keep their own window: {purge}"
    );
    assert!(
        purge["pages_freed"].as_i64().expect("pages freed") >= 0,
        "{purge}"
    );

    assert_eq!(read_rows(&db, "SELECT value FROM samples").len(), 0);
    assert!(
        read_rows(&db, "SELECT ts FROM events").len() >= audit_before,
        "the audit trail is untouched"
    );
    // A series purged empty keeps no bounds from the rows that are gone,
    // and stats stops listing it — what the old aggregate query did by
    // joining.
    assert_tally_matches_samples(&db);
    let replies = history_get(&observer, "home/history/stats").await;
    assert_eq!(
        replies[0].1["series"]
            .as_object()
            .expect("series map")
            .len(),
        0,
        "emptied series are not listed: {}",
        replies[0].1
    );

    // New samples land as before: retention deletes, it never stops writing.
    put(&power, json!(4.5)).await;
    rows_eventually(&db, "SELECT value FROM samples", 1, Duration::from_secs(20)).await;
    assert_tally_matches_samples(&db);

    sup.shutdown();
}

/// The scheduled integrity check: a healthy store reports `integrity-ok`
/// with its duration; a store whose pages are corrupted on disk — the
/// failure SQLite itself never notices — reports `integrity-failed` with
/// what the check found. The schedule is a parameter and applies live.
#[tokio::test(flavor = "multi_thread")]
async fn integrity_check_reports_corruption() {
    let db = store_path("integrity");
    let (mut sup, observer) = setup(&db).await;
    let events = observer
        .declare_subscriber("home/health/recorder/event")
        .await
        .expect("event subscriber");

    let power = matched_publisher(&observer, "home/state/attic/meter/power").await;
    for value in [1.5, 2.5, 3.5] {
        put(&power, json!(value)).await;
    }
    rows_eventually(&db, "SELECT value FROM samples", 3, Duration::from_secs(20)).await;

    // ~0.36 s schedule: the first check runs an interval after the change.
    config_write(
        &observer,
        "home/config/recorder/integrity_check_hours",
        json!(1e-4),
    )
    .await
    .expect("in-constraint write accepted");
    let ok = await_event(&events, Duration::from_secs(30), |e| {
        e["kind"] == "integrity-ok"
    })
    .await;
    assert!(ok["duration_s"].as_f64().expect("duration") >= 0.0, "{ok}");

    // Corrupt the samples table's root page in the main file: nothing
    // rewrites it from here on (the recorder keeps writing its own health
    // events, so any page it touches would be shadowed by a fresh copy in
    // the WAL). Checkpoint first so the page lives in the main file.
    let rootpage = {
        let conn = Connection::open(&db).expect("open store");
        conn.busy_timeout(Duration::from_secs(5))
            .expect("busy timeout");
        let (busy, _log, _done): (i64, i64, i64) = conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .expect("checkpoint");
        assert_eq!(busy, 0, "checkpoint completed");
        // i64, not u64: SQLite has one integer type and rusqlite 0.40
        // dropped the lossy u64 conversion. The offset is cast once, here.
        let page_size: i64 = conn
            .query_row("PRAGMA page_size", [], |r| r.get(0))
            .expect("page size");
        let root: i64 = conn
            .query_row(
                "SELECT rootpage FROM sqlite_master WHERE name = 'samples'",
                [],
                |r| r.get(0),
            )
            .expect("samples root page");
        ((root - 1) * page_size) as u64
    };
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(&db)
            .expect("open file");
        file.seek(SeekFrom::Start(rootpage + 8))
            .expect("seek into the root page");
        file.write_all(&[0xFF; 64]).expect("scribble");
    }
    let failed = await_event(&events, Duration::from_secs(30), |e| {
        e["kind"] == "integrity-failed"
    })
    .await;
    let errors = failed["errors"].as_array().expect("errors listed");
    assert!(!errors.is_empty() && errors[0] != json!("ok"), "{failed}");

    sup.shutdown();
}

/// (d) The read path returns what was written: a get on
/// home/history/state/{entity}/{aspect} replies the typed rows with
/// timestamps, honoring from/to/limit (zenoh's `;`-separated selector
/// parameters) — limit keeps the newest rows, with or without an explicit
/// window; wildcards fan out to concrete series keys; a malformed
/// selector is an error reply. The events table gets the same query
/// surface at home/history/events: key wildcards filter recorded event
/// keys, from/to (here raw microseconds, not RFC3339) window the range,
/// limit truncates keeping the newest, and cmd envelopes carry their
/// actor into the payload.
#[tokio::test(flavor = "multi_thread")]
async fn read_path_returns_history() {
    let db = store_path("read");
    let (mut sup, observer) = setup(&db).await;

    let power = matched_publisher(&observer, "home/state/attic/meter/power").await;
    for value in [1.5, 2.5, 3.5] {
        put(&power, json!(value)).await;
    }
    rows_eventually(
        &db,
        "SELECT value FROM history WHERE entity = 'meter'",
        3,
        Duration::from_secs(20),
    )
    .await;

    // The full series, ascending, typed, tagged with the room.
    let replies = history_get(&observer, "home/history/state/meter/power").await;
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].0, "home/history/state/meter/power");
    let rows = replies[0].1.as_array().expect("reply is an array").clone();
    assert_eq!(rows.len(), 3);
    let timestamps: Vec<&str> = rows
        .iter()
        .map(|r| r["ts"].as_str().expect("ts is a string"))
        .collect();
    for (row, expected) in rows.iter().zip([1.5, 2.5, 3.5]) {
        assert_eq!(row["value"], json!(expected));
        assert_eq!(row["room"], json!("attic"));
        let ts = row["ts"].as_str().expect("ts is a string");
        // RFC3339 UTC: 2026-07-04T19:00:00.123456+00:00
        assert!(
            ts.len() == 32 && &ts[10..11] == "T" && ts.ends_with("+00:00"),
            "RFC3339 UTC timestamp: {ts}"
        );
    }
    let mut sorted = timestamps.clone();
    sorted.sort();
    assert_eq!(timestamps, sorted, "rows are ascending");

    // limit keeps the most recent rows in range.
    let replies = history_get(&observer, "home/history/state/meter/power?limit=2").await;
    let values: Vec<&Value> = replies[0]
        .1
        .as_array()
        .expect("array")
        .iter()
        .map(|r| &r["value"])
        .collect();
    assert_eq!(values, vec![&json!(2.5), &json!(3.5)]);

    // ...including inside an explicit from/to window: the newest in the
    // window, never its far end. A small limit is how a caller asks "what
    // has this been doing lately", and answering with the oldest rows makes
    // a live series look dead.
    let selector = format!(
        "home/history/state/meter/power?from={};to={};limit=2",
        timestamps[0], timestamps[2]
    );
    let replies = history_get(&observer, &selector).await;
    let values: Vec<&Value> = replies[0]
        .1
        .as_array()
        .expect("array")
        .iter()
        .map(|r| &r["value"])
        .collect();
    assert_eq!(
        values,
        vec![&json!(2.5), &json!(3.5)],
        "newest in the window"
    );

    // from narrows the range (reusing a reply timestamp verbatim).
    let selector = format!("home/history/state/meter/power?from={}", timestamps[1]);
    let replies = history_get(&observer, &selector).await;
    let values: Vec<&Value> = replies[0]
        .1
        .as_array()
        .expect("array")
        .iter()
        .map(|r| &r["value"])
        .collect();
    assert_eq!(values, vec![&json!(2.5), &json!(3.5)]);

    // A wildcard fans out to concrete series keys.
    let replies = history_get(&observer, "home/history/state/meter/*").await;
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].0, "home/history/state/meter/power");

    // A malformed selector is an error reply, observable to the caller.
    let replies = observer
        .get("home/history/state/meter/power?from=garbage")
        .await
        .expect("history query");
    let reply = replies.recv_async().await.expect("a reply");
    let err = reply.result().expect_err("malformed from rejected");
    assert!(
        String::from_utf8_lossy(&err.payload().to_bytes()).contains("garbage"),
        "error names the violation"
    );

    // The events surface: home/history/events replies one message, a JSON
    // array of {ts, key, payload}. Supervisor health transitions already
    // landed during setup(), so health events are present without any
    // extra traffic.
    let replies = history_get(&observer, "home/history/events").await;
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].0, "home/history/events");
    let rows = replies[0].1.as_array().expect("reply is an array").clone();
    assert!(
        rows.iter().any(|r| r["key"]
            .as_str()
            .expect("key is a string")
            .starts_with("home/health/")),
        "health events present: {rows:?}"
    );
    for row in &rows {
        assert!(
            row["ts"].is_i64(),
            "events ts is raw microseconds, not RFC3339: {row}"
        );
    }

    // Three cmd envelopes under one key prefix, spaced so from/to can
    // bracket the middle one unambiguously.
    let key_a = "home/cmd/attic/limitprobe/a";
    let key_b = "home/cmd/attic/limitprobe/b";
    let key_c = "home/cmd/attic/limitprobe/c";
    let pub_a = matched_publisher(&observer, key_a).await;
    let pub_b = matched_publisher(&observer, key_b).await;
    let pub_c = matched_publisher(&observer, key_c).await;
    put(
        &pub_a,
        json!({"value": true, "priority": "automation", "actor": "seq-a"}),
    )
    .await;
    rows_eventually(
        &db,
        "SELECT key FROM events WHERE key = 'home/cmd/attic/limitprobe/a'",
        1,
        Duration::from_secs(10),
    )
    .await;

    let before_b = now_us();
    put(
        &pub_b,
        json!({"value": true, "priority": "automation", "actor": "seq-b"}),
    )
    .await;
    rows_eventually(
        &db,
        "SELECT key FROM events WHERE key = 'home/cmd/attic/limitprobe/b'",
        1,
        Duration::from_secs(10),
    )
    .await;
    let after_b = now_us();

    put(
        &pub_c,
        json!({"value": true, "priority": "automation", "actor": "seq-c"}),
    )
    .await;
    rows_eventually(
        &db,
        "SELECT key FROM events WHERE key = 'home/cmd/attic/limitprobe/c'",
        1,
        Duration::from_secs(10),
    )
    .await;

    // A key wildcard narrows to just this prefix, cmd envelopes carrying
    // their actor into the payload, ordered oldest to newest.
    let replies = history_get(
        &observer,
        "home/history/events?key=home/cmd/attic/limitprobe/**",
    )
    .await;
    let rows = replies[0].1.as_array().expect("reply is an array").clone();
    assert_eq!(
        rows.len(),
        3,
        "wildcard filter narrows to the three envelopes: {rows:?}"
    );
    let keys: Vec<&str> = rows
        .iter()
        .map(|r| r["key"].as_str().expect("key"))
        .collect();
    assert_eq!(keys, vec![key_a, key_b, key_c], "oldest to newest");
    assert_eq!(
        rows[1]["payload"]["actor"],
        json!("seq-b"),
        "actor visible in payload"
    );
    assert_eq!(rows[1]["payload"]["value"], json!(true));
    assert_eq!(rows[1]["payload"]["priority"], json!("automation"));

    // from/to windows the range: only the middle envelope falls inside
    // [before_b, after_b].
    let selector = format!(
        "home/history/events?key=home/cmd/attic/limitprobe/**;from={before_b};to={after_b}"
    );
    let replies = history_get(&observer, &selector).await;
    let rows = replies[0].1.as_array().expect("reply is an array").clone();
    assert_eq!(
        rows.len(),
        1,
        "from/to bounds to the bracketed envelope: {rows:?}"
    );
    assert_eq!(rows[0]["key"], json!(key_b));

    // limit truncates keeping the newest rows, still oldest-to-newest.
    let replies = history_get(
        &observer,
        "home/history/events?key=home/cmd/attic/limitprobe/**;limit=2",
    )
    .await;
    let rows = replies[0].1.as_array().expect("reply is an array").clone();
    let keys: Vec<&str> = rows
        .iter()
        .map(|r| r["key"].as_str().expect("key"))
        .collect();
    assert_eq!(
        keys,
        vec![key_b, key_c],
        "limit keeps the newest, oldest-to-newest"
    );

    sup.shutdown();
}

/// The two chart shapes of the samples path: `bucket=<seconds>` folds a
/// window into one point per bucket (mean with min/max for numbers, the
/// last value for anything else, ts the bucket's start), and `changes=1`
/// keeps only the rows at which the value changed — a state's runs. Both
/// fold the whole window before `limit` keeps the newest, which is what
/// lets a chatty series fill a week instead of showing its last hour.
#[tokio::test(flavor = "multi_thread")]
async fn read_path_folds_buckets_and_changes() {
    let db = store_path("fold");
    let (mut sup, observer) = setup(&db).await;

    let power = matched_publisher(&observer, "home/state/attic/meter/power").await;
    for value in [1.5, 2.5, 3.5] {
        put(&power, json!(value)).await;
    }
    let open = matched_publisher(&observer, "home/state/attic/hatch/open").await;
    for value in [true, true, false, false, true] {
        put(&open, json!(value)).await;
    }
    rows_eventually(
        &db,
        "SELECT value FROM history WHERE entity IN ('meter', 'hatch')",
        8,
        Duration::from_secs(20),
    )
    .await;

    // A bucket wider than the test's lifetime folds the series into one
    // point: the mean, its extremes, and the bucket's start — an aligned
    // instant, not any sample's own timestamp.
    let year = 365 * 24 * 3600;
    // A window is needed for a fold: without `from` it starts at the epoch
    // and would make more buckets than a reply carries.
    let window = format!("from=1970-01-02T00:00:00+00:00;bucket={year}");
    let replies = history_get(
        &observer,
        &format!("home/history/state/meter/power?{window}"),
    )
    .await;
    let rows = replies[0].1.as_array().expect("array").clone();
    assert_eq!(rows.len(), 1, "one bucket: {rows:?}");
    assert_eq!(rows[0]["value"], json!(2.5));
    assert_eq!(rows[0]["min"], json!(1.5));
    assert_eq!(rows[0]["max"], json!(3.5));
    assert_eq!(rows[0]["room"], json!("attic"));
    // A whole number of days since the epoch lands on midnight UTC.
    assert!(
        rows[0]["ts"]
            .as_str()
            .expect("ts")
            .ends_with("T00:00:00.000000+00:00"),
        "bucket start is aligned: {}",
        rows[0]["ts"]
    );

    // A bool bucket has no mean: it carries the last value and no extremes.
    let replies = history_get(
        &observer,
        &format!("home/history/state/hatch/open?{window}"),
    )
    .await;
    let rows = replies[0].1.as_array().expect("array").clone();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["value"], json!(true));
    assert!(
        rows[0].get("min").is_none(),
        "no extremes on a bool: {rows:?}"
    );

    // changes=1 collapses runs, keeping the window's first row.
    let replies = history_get(&observer, "home/history/state/hatch/open?changes=1").await;
    let values: Vec<&Value> = replies[0]
        .1
        .as_array()
        .expect("array")
        .iter()
        .map(|r| &r["value"])
        .collect();
    assert_eq!(values, vec![&json!(true), &json!(false), &json!(true)]);

    // limit applies after the fold: the newest runs.
    let replies = history_get(&observer, "home/history/state/hatch/open?changes=1;limit=2").await;
    let values: Vec<&Value> = replies[0]
        .1
        .as_array()
        .expect("array")
        .iter()
        .map(|r| &r["value"])
        .collect();
    assert_eq!(values, vec![&json!(false), &json!(true)]);

    // Malformed or contradictory chart parameters are error replies.
    for (selector, names) in [
        ("home/history/state/meter/power?bucket=0", "positive"),
        ("home/history/state/meter/power?bucket=soon", "integer"),
        (
            "home/history/state/meter/power?bucket=60;changes=1",
            "exclusive",
        ),
        ("home/history/state/meter/power?changes=yes", "changes"),
        // a fold finer than any reply carries is a scan nobody asked for
        ("home/history/state/meter/power?bucket=1", "buckets"),
    ] {
        let replies = observer.get(selector).await.expect("history query");
        let reply = replies.recv_async().await.expect("a reply");
        let err = reply.result().expect_err("rejected");
        assert!(
            String::from_utf8_lossy(&err.payload().to_bytes()).contains(names),
            "{selector}: error names the violation"
        );
    }

    sup.shutdown();
}

/// (e) A version-0 store (one wide samples table, no auto_vacuum) is
/// migrated in place on startup: the rows survive with their series
/// identity and room tags, the file is stamped, and the recorder keeps
/// writing into it.
#[tokio::test(flavor = "multi_thread")]
async fn v0_store_migrates_in_place() {
    let db = store_path("migrate");
    {
        let conn = Connection::open(&db).expect("create v0 store");
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE samples (ts INTEGER NOT NULL, class TEXT NOT NULL,
               room TEXT NOT NULL, entity TEXT NOT NULL, aspect TEXT NOT NULL,
               kind TEXT NOT NULL, value NOT NULL);
             CREATE INDEX samples_series ON samples (class, entity, aspect, ts);
             CREATE TABLE events (ts INTEGER NOT NULL, key TEXT NOT NULL, payload TEXT NOT NULL);
             INSERT INTO samples VALUES (1, 'state', 'attic', 'rover', 'on', 'bool', 1);
             INSERT INTO samples VALUES (2, 'state', 'cellar', 'rover', 'on', 'bool', 0);
             INSERT INTO samples VALUES (3, 'state', 'attic', 'probe', 'temperature', 'number', 21.5);
             INSERT INTO events VALUES (4, 'home/config/x/y', '1');",
        )
        .expect("populate v0 store");
    }
    let (mut sup, observer) = setup(&db).await;

    let rows = read_rows(
        &db,
        "SELECT ts, class, room, entity, aspect, kind, value FROM history ORDER BY ts",
    );
    let row = |ts: i64, room: &str, entity: &str, aspect: &str, kind: &str, value: SqlValue| {
        vec![
            SqlValue::Integer(ts),
            SqlValue::Text("state".into()),
            SqlValue::Text(room.into()),
            SqlValue::Text(entity.into()),
            SqlValue::Text(aspect.into()),
            SqlValue::Text(kind.into()),
            value,
        ]
    };
    assert_eq!(
        rows,
        vec![
            row(1, "attic", "rover", "on", "bool", SqlValue::Integer(1)),
            row(2, "cellar", "rover", "on", "bool", SqlValue::Integer(0)),
            row(
                3,
                "attic",
                "probe",
                "temperature",
                "number",
                SqlValue::Real(21.5)
            ),
        ]
    );
    assert_eq!(
        read_rows(
            &db,
            "SELECT payload FROM events WHERE key = 'home/config/x/y'"
        ),
        vec![vec![SqlValue::Text("1".into())]],
        "events survive untouched"
    );
    assert_eq!(
        read_rows(&db, "PRAGMA user_version"),
        vec![vec![SqlValue::Integer(5)]],
        "a v0 store arrives at the current layout in one start"
    );
    assert_eq!(
        read_rows(&db, "PRAGMA auto_vacuum"),
        vec![vec![SqlValue::Integer(2)]],
        "the VACUUM switched the file to incremental auto_vacuum"
    );

    // The migrated store keeps recording: one series, the room tag moves on.
    let attic = matched_publisher(&observer, "home/state/attic/rover/on").await;
    put(&attic, json!(true)).await;
    rows_eventually(
        &db,
        "SELECT ts FROM history WHERE entity = 'rover'",
        3,
        Duration::from_secs(20),
    )
    .await;
    assert_eq!(
        read_rows(&db, "SELECT count(*) FROM series WHERE entity = 'rover'"),
        vec![vec![SqlValue::Integer(1)]]
    );
    let replies = history_get(&observer, "home/history/state/rover/on").await;
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].1.as_array().expect("array").len(), 3);
    // A v0 store arrives at version 2 in one start, tally included: the
    // rows are inserted by the migration, before the trigger exists.
    assert_tally_matches_samples(&db);

    sup.shutdown();
}

/// (e2) A version-1 store — the layout before the per-series tally — is
/// counted once on startup and answers stats from `series` afterwards.
/// The backfill is the scan version 2 exists to stop doing, paid once
/// where nothing waits on a query timeout.
#[tokio::test(flavor = "multi_thread")]
async fn v1_store_backfills_its_tally() {
    let db = store_path("migrate-v1");
    {
        let conn = Connection::open(&db).expect("create v1 store");
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE series (id INTEGER PRIMARY KEY, class TEXT NOT NULL,
               entity TEXT NOT NULL, aspect TEXT NOT NULL, UNIQUE (class, entity, aspect));
             CREATE TABLE rooms (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE);
             CREATE TABLE samples (series_id INTEGER NOT NULL REFERENCES series (id),
               ts INTEGER NOT NULL, room_id INTEGER NOT NULL REFERENCES rooms (id),
               kind INTEGER NOT NULL, value NOT NULL,
               PRIMARY KEY (series_id, ts)) WITHOUT ROWID;
             CREATE TABLE events (ts INTEGER NOT NULL, key TEXT NOT NULL, payload TEXT NOT NULL);
             INSERT INTO rooms (name) VALUES ('attic');
             INSERT INTO series (class, entity, aspect) VALUES ('state', 'meter', 'power');
             INSERT INTO series (class, entity, aspect) VALUES ('state', 'gauge', 'level');
             INSERT INTO samples VALUES (1, 300, 1, 1, 1.5);
             INSERT INTO samples VALUES (1, 100, 1, 1, 2.5);
             INSERT INTO samples VALUES (1, 200, 1, 1, 3.5);
             INSERT INTO samples VALUES (2, 700, 1, 1, 9);
             PRAGMA user_version=1;",
        )
        .expect("populate v1 store");
    }
    let (mut sup, observer) = setup(&db).await;

    assert_eq!(
        read_rows(&db, "PRAGMA user_version"),
        vec![vec![SqlValue::Integer(5)]]
    );
    // Counted, not guessed: the oldest row of the first series was
    // inserted last, so a tally that took each series' first or last
    // insert for its bounds would be wrong here.
    assert_eq!(
        read_rows(
            &db,
            "SELECT entity, row_count, oldest_ts, newest_ts FROM series ORDER BY id"
        ),
        vec![
            vec![
                SqlValue::Text("meter".into()),
                SqlValue::Integer(3),
                SqlValue::Integer(100),
                SqlValue::Integer(300),
            ],
            vec![
                SqlValue::Text("gauge".into()),
                SqlValue::Integer(1),
                SqlValue::Integer(700),
                SqlValue::Integer(700),
            ],
        ]
    );

    // And the migrated store keeps tallying as it records.
    let power = matched_publisher(&observer, "home/state/attic/meter/power").await;
    put(&power, json!(4.5)).await;
    rows_eventually(&db, "SELECT value FROM samples", 5, Duration::from_secs(20)).await;
    assert_tally_matches_samples(&db);

    let replies = history_get(&observer, "home/history/stats").await;
    let series = replies[0].1["series"].as_object().expect("series map");
    assert_eq!(series["home/history/state/meter/power"]["rows"], json!(4));
    assert_eq!(series["home/history/state/gauge/level"]["rows"], json!(1));

    sup.shutdown();
}

/// (h) The recorder catches up from the core's state mirror (#60): a
/// value published while it is down is in the store once it is back,
/// stamped at the value's own time rather than at recorder start, and a
/// series the previous incarnation recorded live is not duplicated. The
/// publish races the restart; when the fresh incarnation subscribes
/// first the row is recorded live with the same stamp and the assertions
/// hold either way — the seed path is the one that runs in practice, a
/// Python unit taking longer to come up than the put takes to land.
#[tokio::test(flavor = "multi_thread")]
async fn restart_seeds_missed_state_from_the_mirror() {
    let db = store_path("seed");
    let (mut sup, observer) = setup(&db).await;
    let mut recorder = health_watch(&observer, "recorder").await;
    let running = await_health(&mut recorder, Duration::from_secs(10), |h| {
        h.status == HealthStatus::Running
    })
    .await;

    // A series the first incarnation records live.
    let level = matched_publisher(&observer, "home/state/cellar/tank/level").await;
    put(&level, json!(1)).await;
    rows_eventually(
        &db,
        "SELECT ts FROM history WHERE entity = 'tank' AND aspect = 'level'",
        1,
        Duration::from_secs(20),
    )
    .await;

    // Kill the recorder; the supervisor restarts it after its backoff.
    let pid = running.pid.expect("a running recorder has a pid");
    assert_eq!(
        unsafe { libc::kill(pid as i32, libc::SIGKILL) },
        0,
        "kill recorder"
    );
    await_health(&mut recorder, Duration::from_secs(10), |h| {
        h.status == HealthStatus::Backoff
    })
    .await;

    // Published while it is down: a start-time flag that never changes.
    let before_put = now_us();
    let flag = matched_publisher(&observer, "home/state/cellar/tank/available").await;
    put(&flag, json!(true)).await;
    await_mirror(&observer, "home/state/cellar/tank/available", &json!(true)).await;
    let after_put = now_us();
    await_health(&mut recorder, Duration::from_secs(60), |h| {
        h.status == HealthStatus::Running
    })
    .await;

    let rows = rows_eventually(
        &db,
        "SELECT ts, value FROM history WHERE entity = 'tank' AND aspect = 'available'",
        1,
        Duration::from_secs(20),
    )
    .await;
    assert_eq!(rows[0][1], SqlValue::Integer(1));
    let SqlValue::Integer(ts) = rows[0][0] else {
        panic!("ts is an integer");
    };
    assert!(
        ts >= before_put - 1_000_000 && ts <= after_put + 1_000_000,
        "seeded row is stamped at the value's time: {ts} not in [{before_put}, {after_put}]"
    );

    // The seed's rows flush in the same batch as the row above, so the
    // live-recorded series would already show its duplicate.
    let level_rows = read_rows(
        &db,
        "SELECT ts FROM history WHERE entity = 'tank' AND aspect = 'level'",
    );
    assert_eq!(
        level_rows.len(),
        1,
        "a series the store already holds is not seeded again"
    );

    sup.shutdown();
}

/// (l) Forecasts are the one class that does not ride `samples`
/// (docs/design.md, Forecasts). Two issues about the same future instant
/// both survive — which is the whole reason the table exists — a point's
/// declared extent is stored rather than inferred from succession, and
/// the two verification read shapes answer in issues.
#[tokio::test(flavor = "multi_thread")]
async fn forecasts_keep_every_issue_and_answer_in_issues() {
    let db = store_path("forecast");
    let (mut sup, observer) = setup(&db).await;

    // Fixed instants, and deliberately in the past: verification reads
    // forecasts whose valid time has already come, and a test that
    // leans on "now" would answer differently depending on the hour it
    // runs at.
    let noon = "2020-01-01T12:00:00+00:00";
    let one = "2020-01-01T13:00:00+00:00";
    let issue = |issued: &str, noon_value: f64| {
        json!({
            "schema": 1,
            "issued": issued,
            "points": [
                // an interval: it holds for its hour and says so
                {"t": noon, "v": noon_value, "d": 3600.0},
                // an instant: no extent, speaks only for itself
                {"t": one, "v": 9.0},
            ],
        })
    };

    let key = "home/forecast/global/spot/price/nordpool";
    let pub_ = matched_publisher(&observer, key).await;
    put(&pub_, issue("2020-01-01T08:00:00+00:00", 21.0)).await;
    put(&pub_, issue("2020-01-01T09:00:00+00:00", 23.5)).await;

    // Four rows: two issues of two points. On `samples` the second issue
    // would have collided with the first on (series_id, ts).
    let rows = rows_eventually(
        &db,
        "SELECT issued_ts, valid_ts, valid_end, value FROM forecasts ORDER BY issued_ts, valid_ts",
        4,
        Duration::from_secs(20),
    )
    .await;
    assert_eq!(rows.len(), 4, "{rows:?}");

    // The interval carries its extent; the instant carries none.
    let interval_end = &rows[0][2];
    let instant_end = &rows[1][2];
    assert!(
        !matches!(interval_end, SqlValue::Null),
        "an interval stores its end: {rows:?}"
    );
    assert!(
        matches!(instant_end, SqlValue::Null),
        "an instant has no end to store: {rows:?}"
    );

    // The forecast series is tallied like any other, on issue time.
    let series = read_rows(
        &db,
        "SELECT class, entity, aspect, source, row_count FROM series WHERE class = 'forecast'",
    );
    assert_eq!(series.len(), 1, "{series:?}");
    assert_eq!(
        series[0][3],
        SqlValue::Text("nordpool".to_string()),
        "the series is identified by its source too: {series:?}"
    );
    assert_eq!(series[0][4], SqlValue::Integer(4), "{series:?}");

    // `at`: the forecast as it stood. Before the second issue existed,
    // the answer is the first one — which is what verification means.
    let replies = history_get(
        &observer,
        "home/history/forecast/spot/price/nordpool?at=2020-01-01T08:30:00+00:00",
    )
    .await;
    assert_eq!(replies.len(), 1, "{replies:?}");
    let issues = replies[0].1.as_array().expect("issues array").clone();
    assert_eq!(issues.len(), 1, "one issue stood at 08:30: {issues:?}");
    // iso_utc's microsecond form, as the samples path spells a ts.
    assert_eq!(
        issues[0]["issued"],
        json!("2020-01-01T08:00:00.000000+00:00")
    );
    let points = issues[0]["points"].as_array().expect("points");
    assert_eq!(points[0]["v"], json!(21.0), "the older opinion: {points:?}");
    // The wire's own shape, so the SDK's decoder reads it unchanged.
    assert_eq!(issues[0]["schema"], json!(1));
    assert_eq!(points[0]["d"], json!(3600.0));
    assert!(
        points[1].get("d").is_none(),
        "an instant has no d: {points:?}"
    );

    // Default `at` is now, so a bare read is the current forecast.
    let replies = history_get(&observer, "home/history/forecast/spot/price/nordpool").await;
    let issues = replies[0].1.as_array().expect("issues array").clone();
    assert_eq!(issues.len(), 1);
    assert_eq!(
        issues[0]["issued"],
        json!("2020-01-01T09:00:00.000000+00:00")
    );

    // The verification window: every issue that spoke about noon, each
    // carrying only its overlapping points.
    let replies = history_get(
        &observer,
        "home/history/forecast/spot/price/nordpool\
         ?valid_from=2020-01-01T12:00:00+00:00;valid_to=2020-01-01T13:00:00+00:00",
    )
    .await;
    let issues = replies[0].1.as_array().expect("issues array").clone();
    assert_eq!(issues.len(), 2, "both opinions about noon: {issues:?}");
    let said: Vec<&Value> = issues
        .iter()
        .map(|i| &i["points"].as_array().expect("points")[0]["v"])
        .collect();
    assert_eq!(said, vec![&json!(21.0), &json!(23.5)]);
    for issue in &issues {
        assert_eq!(
            issue["points"].as_array().expect("points").len(),
            1,
            "the 13:00 instant is outside [12:00, 13:00): {issue}"
        );
    }

    // The two shapes are exclusive rather than quietly one winning.
    let replies = observer
        .get("home/history/forecast/spot/price/nordpool?at=2020-01-01T08:00:00+00:00;valid_from=2020-01-01T12:00:00+00:00")
        .await
        .expect("query");
    let reply = replies.recv_async().await.expect("a reply");
    assert!(
        reply.result().is_err(),
        "at and a window together are refused"
    );

    // A wildcard over history keeps fanning out over sample series alone,
    // so the two reply shapes never arrive mixed.
    let replies = history_get(&observer, "home/history/**").await;
    assert!(
        replies.iter().all(|(key, _)| !key.contains("/forecast/")),
        "a history wildcard must not mix in forecast issues: {replies:?}"
    );

    sup.shutdown();
}

/// (e4) A version-4 store carries forecast series with no source: the
/// segment did not exist when they were recorded, and that migration left
/// them empty. Empty is not a key segment, so building the reply key for
/// one raised inside the query callback — which sends no reply at all,
/// and the caller reads that as "no data". The `SELECT` is unfiltered, so
/// the one legacy series took down the answer for every OTHER forecast
/// series in the store, including correctly-sourced ones with rows.
/// Found on a house upgraded to 0.15.0.
#[tokio::test(flavor = "multi_thread")]
async fn v4_store_names_the_sources_it_left_empty() {
    let db = store_path("migrate-v4");
    {
        let conn = Connection::open(&db).expect("create v4 store");
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE series (id INTEGER PRIMARY KEY, class TEXT NOT NULL,
               entity TEXT NOT NULL, aspect TEXT NOT NULL,
               source TEXT NOT NULL DEFAULT '',
               row_count INTEGER NOT NULL DEFAULT 0, oldest_ts INTEGER, newest_ts INTEGER,
               UNIQUE (class, entity, aspect, source));
             CREATE TABLE rooms (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE);
             CREATE TABLE samples (series_id INTEGER NOT NULL REFERENCES series (id),
               ts INTEGER NOT NULL, room_id INTEGER NOT NULL REFERENCES rooms (id),
               kind INTEGER NOT NULL, value NOT NULL,
               PRIMARY KEY (series_id, ts)) WITHOUT ROWID;
             CREATE TABLE forecasts (series_id INTEGER NOT NULL REFERENCES series (id),
               issued_ts INTEGER NOT NULL, valid_ts INTEGER NOT NULL, valid_end INTEGER,
               room_id INTEGER NOT NULL REFERENCES rooms (id), value NOT NULL,
               PRIMARY KEY (series_id, issued_ts, valid_ts)) WITHOUT ROWID;
             CREATE TABLE events (ts INTEGER NOT NULL, key TEXT NOT NULL, payload TEXT NOT NULL);
             INSERT INTO rooms (name) VALUES ('global');
             -- The legacy series, sorted ahead of the sourced one: it is
             -- what the read loop reached first on the house.
             INSERT INTO series (id, class, entity, aspect, source, row_count)
               VALUES (1, 'forecast', 'outdoor', 'air_temperature', '', 1),
                      (2, 'forecast', 'spot', 'price', 'nordpool', 1),
                      (3, 'state', 'meter', 'power', '', 0);
             INSERT INTO forecasts VALUES (1, 1577865600000000, 1577880000000000, NULL, 1, 4.0),
                                          (2, 1577865600000000, 1577880000000000, 1577883600000000, 1, 21.0);
             PRAGMA user_version=4;",
        )
        .expect("populate v4 store");
    }
    let (mut sup, observer) = setup(&db).await;

    assert_eq!(
        read_rows(&db, "PRAGMA user_version"),
        vec![vec![SqlValue::Integer(5)]],
        "the store reports the layout it now has"
    );
    // Only forecast series are named: every other class has an empty
    // source by definition and never puts it in a key.
    assert_eq!(
        read_rows(&db, "SELECT source FROM series ORDER BY id"),
        vec![
            vec![SqlValue::Text("_unknown".to_string())],
            vec![SqlValue::Text("nordpool".to_string())],
            vec![SqlValue::Text(String::new())],
        ],
        "the sourceless forecast gets the reserved name, the state series none"
    );

    // The regression itself: the sourced series answers, rather than
    // being hidden behind the legacy one the loop reaches first.
    let replies = history_get(
        &observer,
        "home/history/forecast/spot/price/nordpool?at=2020-01-01T09:00:00+00:00",
    )
    .await;
    assert_eq!(replies.len(), 1, "{replies:?}");
    let issues = replies[0].1.as_array().expect("issues array").clone();
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert_eq!(issues[0]["points"][0]["v"], json!(21.0));

    // And the legacy rows are readable rather than merely inert — which
    // is the whole reason for a reserved name over skipping them.
    let replies = history_get(
        &observer,
        "home/history/forecast/outdoor/air_temperature/_unknown?at=2020-01-01T09:00:00+00:00",
    )
    .await;
    let issues = replies[0].1.as_array().expect("issues array").clone();
    assert_eq!(issues[0]["points"][0]["v"], json!(4.0), "{issues:?}");

    sup.shutdown();
}

/// (e3) A version-2 store — the layout before forecasts — gains the new
/// table AND the series `source` on the next start. The table is additive
/// and needs no backfill; the source is not, because its uniqueness moved
/// and a table-level UNIQUE cannot be dropped in place. The path every
/// existing house takes on this upgrade, and the one where "additive"
/// quietly meaning "unreachable" would not show up until a producer
/// published.
#[tokio::test(flavor = "multi_thread")]
async fn v2_store_gains_the_forecast_table() {
    let db = store_path("migrate-v2");
    {
        let conn = Connection::open(&db).expect("create v2 store");
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE series (id INTEGER PRIMARY KEY, class TEXT NOT NULL,
               entity TEXT NOT NULL, aspect TEXT NOT NULL,
               row_count INTEGER NOT NULL DEFAULT 0, oldest_ts INTEGER, newest_ts INTEGER,
               UNIQUE (class, entity, aspect));
             CREATE TABLE rooms (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE);
             CREATE TABLE samples (series_id INTEGER NOT NULL REFERENCES series (id),
               ts INTEGER NOT NULL, room_id INTEGER NOT NULL REFERENCES rooms (id),
               kind INTEGER NOT NULL, value NOT NULL,
               PRIMARY KEY (series_id, ts)) WITHOUT ROWID;
             CREATE TABLE events (ts INTEGER NOT NULL, key TEXT NOT NULL, payload TEXT NOT NULL);
             INSERT INTO rooms (name) VALUES ('attic');
             INSERT INTO series (class, entity, aspect, row_count, oldest_ts, newest_ts)
               VALUES ('state', 'meter', 'power', 1, 100, 100);
             INSERT INTO samples VALUES (1, 100, 1, 1, 2.5);
             PRAGMA user_version=2;",
        )
        .expect("populate v2 store");
    }
    let (mut sup, observer) = setup(&db).await;

    assert_eq!(
        read_rows(&db, "PRAGMA user_version"),
        vec![vec![SqlValue::Integer(5)]],
        "the store reports the layout it now has"
    );
    // The existing series is untouched — an upgrade is not a rewrite —
    // and it carries the empty source every non-forecast series has.
    assert_eq!(
        read_rows(
            &db,
            "SELECT row_count, source FROM series WHERE entity = 'meter'"
        ),
        vec![vec![SqlValue::Integer(1), SqlValue::Text(String::new())]],
        "an existing tally survives, with an empty source"
    );
    // The migration writes its own CREATE TABLE, so drift from SCHEMA is
    // the failure mode. Pinned as a literal rather than compared against
    // a second live store: spawning one inside this test contends with
    // the supervisor already running here, and the columns are the thing
    // worth pinning anyway.
    assert_eq!(
        read_rows(&db, "SELECT name FROM pragma_table_info('series')"),
        [
            "id",
            "class",
            "entity",
            "aspect",
            "source",
            "row_count",
            "oldest_ts",
            "newest_ts"
        ]
        .iter()
        .map(|c| vec![SqlValue::Text((*c).to_string())])
        .collect::<Vec<_>>(),
        "a migrated series must be shaped like a fresh one"
    );
    // And the moved uniqueness actually took: two sources, one aspect.
    assert!(
        Connection::open(&db)
            .expect("open migrated store")
            .execute_batch(
                "INSERT INTO series (class, entity, aspect, source)
                   VALUES ('forecast', 'spot', 'price', 'smhi'),
                          ('forecast', 'spot', 'price', 'yr');"
            )
            .is_ok(),
        "two providers for one aspect are two series, not a conflict"
    );

    // And the new table is not merely present but written and read: a
    // published forecast lands, and the series is tallied beside the
    // state one it has never met.
    let key = "home/forecast/global/spot/price/nordpool";
    let pub_ = matched_publisher(&observer, key).await;
    put(
        &pub_,
        json!({
            "schema": 1,
            "issued": "2020-01-01T08:00:00+00:00",
            "points": [{"t": "2020-01-01T12:00:00+00:00", "v": 21.0, "d": 3600.0}],
        }),
    )
    .await;
    rows_eventually(
        &db,
        "SELECT value FROM forecasts",
        1,
        Duration::from_secs(20),
    )
    .await;
    let replies = history_get(&observer, "home/history/forecast/spot/price/nordpool").await;
    let issues = replies[0].1.as_array().expect("issues array").clone();
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert_eq!(issues[0]["points"][0]["v"], json!(21.0));

    sup.shutdown();
}
