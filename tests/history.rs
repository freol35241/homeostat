//! `ctx.history`: a unit reading a subscribed series' recorded past
//! (docs/design.md#reading-the-recorded-past).
//!
//! The fixture's unit publishes three amounts, reads them back raw and in
//! buckets, and publishes what it read. This runs the SDK's selector
//! against the real recorder, so a window or bucket the recorder parses
//! differently from how the SDK writes it fails here.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use homeostat::bus::HealthStatus;
use serde_json::json;

use common::{await_health, await_mirror, health_watch, Supervisor};

const FIXTURE: &str = "tests/fixtures/house_history";
const GAUGE: &str = "home/state/garden/rain_gauge";

fn store_path() -> PathBuf {
    let path = std::env::temp_dir().join(format!("homeostat-history-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    path
}

/// The three amounts come back as three rows, and the buckets' sums and
/// counts add up to what was published.
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
    await_mirror(&observer, &format!("{GAUGE}/total"), &json!(2.0)).await;
    await_mirror(&observer, &format!("{GAUGE}/count"), &json!(3)).await;

    sup.shutdown();
    let _ = std::fs::remove_file(&db);
}
