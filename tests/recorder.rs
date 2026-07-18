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

use common::{await_health, health_watch, Supervisor};

const FIXTURE: &str = "tests/fixture_house_recorder";

type Sub = Subscriber<FifoChannelHandler<Sample>>;
type Publisher = zenoh::pubsub::Publisher<'static>;

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

/// Declares a publisher and waits until a subscriber matches it, so
/// nothing this publisher puts is ever write-side filtered.
async fn matched_publisher(session: &zenoh::Session, key: &str) -> Publisher {
    let publisher = session
        .declare_publisher(key.to_string())
        .await
        .expect("publisher");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let status = publisher.matching_status().await.expect("matching status");
        if status.matching() {
            return publisher;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no subscriber matched {}",
            publisher.key_expr()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn put(publisher: &Publisher, value: Value) {
    publisher.put(value.to_string()).await.expect("state put");
}

/// Rows for `sql` against the store, empty while the store isn't there yet.
fn read_rows(db: &Path, sql: &str) -> Vec<Vec<SqlValue>> {
    let Ok(conn) = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)
    else {
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

/// Writes a parameter through the core's query-with-payload write path.
async fn config_write(session: &zenoh::Session, key: &str, value: Value) {
    let replies = session
        .get(key)
        .payload(value.to_string())
        .await
        .expect("config write query");
    let reply = replies.recv_async().await.expect("config write reply");
    assert!(reply.result().is_ok(), "config write accepted");
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
        "SELECT class, room, aspect, kind, value FROM samples \
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
    put(&cmd, json!({"value": false, "priority": "automation", "actor": "test"})).await;
    let rows = rows_eventually(
        &db,
        "SELECT kind, value FROM samples WHERE class = 'cmd' AND entity = 'probe'",
        1,
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(rows, vec![vec![
        SqlValue::Text("bool".into()),
        SqlValue::Integer(0),
    ]]);
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
        read_rows(&db, "SELECT * FROM samples WHERE aspect = 'brightness'").is_empty(),
        "envelope-less cmd payload became a row"
    );

    // An accepted config edit lands in the events audit table (rejects
    // never reach the bus, so they can't land — pinned in step 4).
    config_write(&observer, "home/config/evening_lights/off_time", json!("21:30")).await;
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
        read_rows(&db, "SELECT * FROM samples WHERE aspect = 'color'").is_empty(),
        "non-scalar payload became a row"
    );

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
        "SELECT room FROM samples WHERE entity = 'rover'",
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
        "SELECT class, entity, aspect, room, value FROM samples \
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
        "SELECT value FROM samples WHERE entity = 'gauge'",
        1,
        Duration::from_secs(20),
    )
    .await;

    // Kill the backend: the store file becomes unwritable.
    let mut perms = std::fs::metadata(&db).expect("store exists").permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&db, perms.clone()).expect("chmod store read-only");

    let before_put = now_us();
    put(&gauge, json!(2)).await;
    await_event(&events, Duration::from_secs(30), |e| e["kind"] == "backend-outage").await;
    let after_outage = now_us();

    // Nothing landed while down (the store is still readable).
    assert_eq!(
        read_rows(&db, "SELECT value FROM samples WHERE entity = 'gauge'").len(),
        1,
        "sample leaked into an unwritable store"
    );

    // More state during the outage joins the buffer; only one outage
    // event marks the whole transition.
    put(&gauge, json!(3)).await;

    // Restore the backend.
    perms.set_readonly(false);
    std::fs::set_permissions(&db, perms).expect("chmod store writable");
    let restored =
        await_event(&events, Duration::from_secs(30), |e| e["kind"] == "backend-restored").await;
    assert!(
        restored["flushed"].as_i64().expect("flushed count") >= 2,
        "restored event reports the flush: {restored}"
    );
    assert_eq!(restored["dropped"], json!(0), "nothing overflowed: {restored}");

    // The buffer flushed in order, and the buffered samples carry their
    // receive-time timestamps — the outage is invisible in the data.
    let rows = rows_eventually(
        &db,
        "SELECT value, ts FROM samples WHERE entity = 'gauge' ORDER BY ts",
        3,
        Duration::from_secs(10),
    )
    .await;
    let values: Vec<&SqlValue> = rows.iter().map(|r| &r[0]).collect();
    assert_eq!(
        values,
        vec![&SqlValue::Integer(1), &SqlValue::Integer(2), &SqlValue::Integer(3)]
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

/// (d) The read path returns what was written: a get on
/// home/history/state/{entity}/{aspect} replies the typed rows with
/// timestamps, honoring from/to/limit (zenoh's `;`-separated selector
/// parameters); wildcards fan out to concrete series keys; a malformed
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
        "SELECT value FROM samples WHERE entity = 'meter'",
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
    let values: Vec<&Value> = replies[0].1.as_array().expect("array").iter()
        .map(|r| &r["value"])
        .collect();
    assert_eq!(values, vec![&json!(2.5), &json!(3.5)]);

    // from narrows the range (reusing a reply timestamp verbatim).
    let selector = format!("home/history/state/meter/power?from={}", timestamps[1]);
    let replies = history_get(&observer, &selector).await;
    let values: Vec<&Value> = replies[0].1.as_array().expect("array").iter()
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
        rows.iter()
            .any(|r| r["key"].as_str().expect("key is a string").starts_with("home/health/")),
        "health events present: {rows:?}"
    );
    for row in &rows {
        assert!(row["ts"].is_i64(), "events ts is raw microseconds, not RFC3339: {row}");
    }

    // Three cmd envelopes under one key prefix, spaced so from/to can
    // bracket the middle one unambiguously.
    let key_a = "home/cmd/attic/limitprobe/a";
    let key_b = "home/cmd/attic/limitprobe/b";
    let key_c = "home/cmd/attic/limitprobe/c";
    let pub_a = matched_publisher(&observer, key_a).await;
    let pub_b = matched_publisher(&observer, key_b).await;
    let pub_c = matched_publisher(&observer, key_c).await;
    put(&pub_a, json!({"value": true, "priority": "automation", "actor": "seq-a"})).await;
    rows_eventually(
        &db,
        "SELECT key FROM events WHERE key = 'home/cmd/attic/limitprobe/a'",
        1,
        Duration::from_secs(10),
    )
    .await;

    let before_b = now_us();
    put(&pub_b, json!({"value": true, "priority": "automation", "actor": "seq-b"})).await;
    rows_eventually(
        &db,
        "SELECT key FROM events WHERE key = 'home/cmd/attic/limitprobe/b'",
        1,
        Duration::from_secs(10),
    )
    .await;
    let after_b = now_us();

    put(&pub_c, json!({"value": true, "priority": "automation", "actor": "seq-c"})).await;
    rows_eventually(
        &db,
        "SELECT key FROM events WHERE key = 'home/cmd/attic/limitprobe/c'",
        1,
        Duration::from_secs(10),
    )
    .await;

    // A key wildcard narrows to just this prefix, cmd envelopes carrying
    // their actor into the payload, ordered oldest to newest.
    let replies =
        history_get(&observer, "home/history/events?key=home/cmd/attic/limitprobe/**").await;
    let rows = replies[0].1.as_array().expect("reply is an array").clone();
    assert_eq!(rows.len(), 3, "wildcard filter narrows to the three envelopes: {rows:?}");
    let keys: Vec<&str> = rows.iter().map(|r| r["key"].as_str().expect("key")).collect();
    assert_eq!(keys, vec![key_a, key_b, key_c], "oldest to newest");
    assert_eq!(rows[1]["payload"]["actor"], json!("seq-b"), "actor visible in payload");
    assert_eq!(rows[1]["payload"]["value"], json!(true));
    assert_eq!(rows[1]["payload"]["priority"], json!("automation"));

    // from/to windows the range: only the middle envelope falls inside
    // [before_b, after_b].
    let selector = format!(
        "home/history/events?key=home/cmd/attic/limitprobe/**;from={before_b};to={after_b}"
    );
    let replies = history_get(&observer, &selector).await;
    let rows = replies[0].1.as_array().expect("reply is an array").clone();
    assert_eq!(rows.len(), 1, "from/to bounds to the bracketed envelope: {rows:?}");
    assert_eq!(rows[0]["key"], json!(key_b));

    // limit truncates keeping the newest rows, still oldest-to-newest.
    let replies =
        history_get(&observer, "home/history/events?key=home/cmd/attic/limitprobe/**;limit=2")
            .await;
    let rows = replies[0].1.as_array().expect("reply is an array").clone();
    let keys: Vec<&str> = rows.iter().map(|r| r["key"].as_str().expect("key")).collect();
    assert_eq!(keys, vec![key_b, key_c], "limit keeps the newest, oldest-to-newest");

    sup.shutdown();
}
