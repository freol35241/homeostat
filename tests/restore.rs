//! `ctx.restore`: a unit reading its own last published value back from
//! the recorder (#83, docs/design.md, Restoring a unit's own last value).
//!
//! The core's state mirror is in-memory, so a core restart — every version
//! upgrade is one — empties it and `subscribe`'s catch-up has nothing to
//! replay. A latch is the case where that matters: nothing can recompute a
//! decision somebody made, so without this the mode falls to its code
//! default and silently disagrees with the house until a human notices.
//!
//! The fixture reproduces exactly that sequence — command the latch, take
//! the core down, bring it back on the same store — and asserts the
//! decision survives. The store outlives the supervisor because it is a
//! file named by RECORDER_DB, as in recorder.rs.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use homeostat::bus::HealthStatus;
use serde_json::{json, Value};

use common::{await_health, await_mirror, health_watch, matched_publisher, Supervisor};

const FIXTURE: &str = "tests/fixture_house_restore";
const LATCH: &str = "home/state/global/night_mode/on";

fn store_path(test: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "homeostat-restore-{test}-{}.db",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    path
}

/// Spawns the fixture against `db` and waits for both units.
async fn setup(db: &Path) -> (Supervisor, zenoh::Session) {
    let sup = Supervisor::spawn_with_env(
        FIXTURE,
        &[("RECORDER_DB", db.to_str().expect("utf-8 path"))],
    );
    let observer = sup.observer().await;
    for unit in ["recorder", "latches"] {
        let mut health = health_watch(&observer, unit).await;
        await_health(&mut health, Duration::from_secs(120), |h| {
            h.status == HealthStatus::Running
        })
        .await;
    }
    (sup, observer)
}

/// Polls the recorder until the latch's history holds `expected` as its
/// newest row — the row `restore` will read after the restart.
async fn await_recorded(session: &zenoh::Session, expected: &Value) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let replies = session
            .get("home/history/state/night_mode/on?limit=1")
            .await
            .expect("history query");
        while let Ok(reply) = replies.recv_async().await {
            if let Ok(sample) = reply.result() {
                let rows: Value =
                    serde_json::from_slice(&sample.payload().to_bytes()).expect("rows are JSON");
                if rows
                    .as_array()
                    .and_then(|r| r.last())
                    .map(|row| &row["value"])
                    == Some(expected)
                {
                    return;
                }
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "recorder never held {expected} for the latch"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// The incident from #83, reproduced: a latch commanded on, the core
/// restarted, and the decision still standing afterwards — where before it
/// fell to the code default and stayed there.
#[tokio::test(flavor = "multi_thread")]
async fn a_latch_restores_its_decision_across_a_core_restart() {
    let db = store_path("decision");
    let (mut sup, observer) = setup(&db).await;

    // Nothing recorded yet, so the latch starts on its code default.
    await_mirror(&observer, LATCH, &json!(false)).await;

    let commander = matched_publisher(&observer, "home/cmd/global/night_mode/on").await;
    commander
        .put(
            serde_json::to_vec(&json!({"value": true, "priority": "family", "actor": "test"}))
                .expect("envelope"),
        )
        .await
        .expect("command put");
    await_mirror(&observer, LATCH, &json!(true)).await;
    await_recorded(&observer, &json!(true)).await;

    // The core goes down and comes back: the mirror is empty, the store is
    // not. Everything the latch knows now comes from `restore`.
    sup.shutdown();
    drop(observer);
    let (mut sup, observer) = setup(&db).await;
    await_mirror(&observer, LATCH, &json!(true)).await;

    sup.shutdown();
    let _ = std::fs::remove_file(&db);
}
