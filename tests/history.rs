//! `ctx.history`: a unit reading a subscribed series' recorded past
//! (docs/design.md#reading-the-recorded-past), stamped publishes
//! (docs/design.md#timestamps) and interval totals
//! (docs/design.md#totals-over-intervals).
//!
//! The fixture's unit publishes three hourly amounts, each stamped with the
//! start of its hour, reads them back raw and in buckets, and publishes
//! what it read. This runs the SDK's selector against the real recorder,
//! so a window or bucket the recorder parses differently from how the SDK
//! writes it fails here, and so does a stamp that does not reach the store
//! to the microsecond.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use homeostat::bus::HealthStatus;
use serde_json::json;

use common::{await_health, await_mirror, health_watch, Supervisor};

/// The newest amount is stamped at the start of the previous hour, so the
/// mirror must report it as between one and two of these old.
const AN_HOUR_S: f64 = 3600.0;

const FIXTURE: &str = "tests/fixtures/house_history";
const GAUGE: &str = "home/state/garden/rain_gauge";

fn store_path() -> PathBuf {
    let path = std::env::temp_dir().join(format!("homeostat-history-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    path
}

/// The three amounts come back as three rows at their hours' starts, the
/// buckets' sums and counts add up to what was published, a window total
/// counts a straddling hour in proportion and refuses a window the hours
/// do not reach, and the mirror ages the newest amount from its stamp.
#[tokio::test(flavor = "multi_thread")]
async fn a_unit_reads_back_what_the_recorder_holds() {
    let db = store_path();
    let mut sup = Supervisor::spawn_with_env(
        FIXTURE,
        &[("RECORDER_DB", db.to_str().expect("utf-8 path"))],
    );
    let observer = sup.observer().await;
    let mut health = health_watch(&observer, "rain").await;
    await_health(&mut health, Duration::from_mins(2), |h| {
        h.status == HealthStatus::Running
    })
    .await;

    await_mirror(&observer, &format!("{GAUGE}/rows"), &json!(3)).await;
    await_mirror(&observer, &format!("{GAUGE}/exact"), &json!(true)).await;
    await_mirror(&observer, &format!("{GAUGE}/total"), &json!(2.0)).await;
    await_mirror(&observer, &format!("{GAUGE}/count"), &json!(3)).await;
    // Half of the first hour's 0.5, then 0.0 and 1.5.
    await_mirror(&observer, &format!("{GAUGE}/window"), &json!(1.75)).await;
    await_mirror(&observer, &format!("{GAUGE}/short"), &json!("short")).await;

    let replies = observer
        .get(format!("{GAUGE}/precipitation"))
        .await
        .expect("mirror get");
    let reply = replies.recv_async().await.expect("a mirror reply");
    let sample = reply.result().expect("an ok reply");
    let value: serde_json::Value =
        serde_json::from_slice(&sample.payload().to_bytes()).expect("JSON payload");
    assert_eq!(value, json!(1.5), "the newest stamp's value is held");
    let age_s: f64 = String::from_utf8(
        sample
            .attachment()
            .expect("the mirror attaches the age")
            .to_bytes()
            .to_vec(),
    )
    .expect("utf-8 age")
    .parse()
    .expect("a decimal age");
    assert!(
        (AN_HOUR_S..2.0 * AN_HOUR_S).contains(&age_s),
        "aged from the newest stamp, got {age_s} s"
    );

    sup.shutdown();
    let _ = std::fs::remove_file(&db);
}
