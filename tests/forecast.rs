//! Forecast bus class, end to end (docs/design.md, Forecasts).
//!
//! 1. A unit may publish `home/forecast/{room}/{entity}/{aspect}` for an
//!    entity it binds — the plan accepts the class and the key shape.
//! 2. The payload is the SDK's: `schema`, `issued`, and irregular `points`
//!    ascending in time. Irregular is the point — a regular grid could not
//!    carry the hourly-then-three-hourly shape real sources publish.
//! 3. The core MIRRORS the class, which is what makes a forecast usable at
//!    all: a day-ahead curve is published once a day, so a consumer that
//!    starts after the publish must still get it. This is the property the
//!    whole class rests on, so it is read here through a session that was
//!    not connected when the unit published.

mod common;

use std::time::Duration;

use homeostat::bus::HealthStatus;
use serde_json::Value;

use common::{await_health, health_watch, Supervisor};

const FIXTURE: &str = "tests/fixture_house_forecast";
const FORECAST_KEY: &str = "home/forecast/global/spot_price/price";

/// Queries a key and returns the first ok reply's payload, retrying until
/// one arrives — the mirror answers only once it has seen the put.
async fn mirror_read_eventually(session: &zenoh::Session, key: &str) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let replies = session.get(key).await.expect("mirror query");
        while let Ok(reply) = replies.recv_async().await {
            if let Ok(sample) = reply.result() {
                return serde_json::from_slice(&sample.payload().to_bytes())
                    .expect("mirror reply is JSON");
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "mirror never answered for {key}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_forecast_is_published_and_mirrored_for_a_late_joiner() {
    let mut sup = Supervisor::spawn(FIXTURE);
    let observer = sup.observer().await;

    let mut seer = health_watch(&observer, "seer").await;
    await_health(&mut seer, Duration::from_secs(120), |h| {
        h.status == HealthStatus::Running
    })
    .await;

    // A session opened after the unit published: the only way it can see
    // the forecast is the core's mirror. A day-ahead curve arrives once a
    // day, so without this a consumer restarting mid-horizon would run
    // blind until the next issue.
    let late_joiner = sup.observer().await;
    let payload = mirror_read_eventually(&late_joiner, FORECAST_KEY).await;

    assert_eq!(payload["schema"], 1, "{payload}");
    assert!(
        payload["issued"].as_str().is_some_and(|s| s.contains('T')),
        "issued is an RFC3339 instant: {payload}"
    );

    let points = payload["points"].as_array().expect("points array");
    assert_eq!(points.len(), 6, "{payload}");

    // Ascending, and irregular: hourly to +3h, then three-hourly. The gaps
    // are the shape the class exists to carry.
    let times: Vec<String> = points
        .iter()
        .map(|p| p["t"].as_str().expect("point time").to_string())
        .collect();
    let mut sorted = times.clone();
    sorted.sort();
    assert_eq!(times, sorted, "points ascend in time: {payload}");

    let parsed: Vec<i64> = times
        .iter()
        .map(|t| chrono_free_epoch_seconds(t).expect("point time parses as RFC3339 with an offset"))
        .collect();
    let gaps: Vec<i64> = parsed.windows(2).map(|w| w[1] - w[0]).collect();
    assert_eq!(
        gaps,
        vec![3600, 3600, 3600, 10800, 10800],
        "the horizon is irregular by design: {payload}"
    );

    assert_eq!(points[0]["v"], 1.20, "{payload}");

    sup.shutdown();
}

/// Epoch seconds from an RFC3339 instant with a UTC offset, without adding
/// a date dependency for one assertion: the fixture publishes `Z`-less
/// ISO with a `+00:00` offset, which is all this needs to handle.
fn chrono_free_epoch_seconds(ts: &str) -> Option<i64> {
    let (date, rest) = ts.split_once('T')?;
    let time = rest.split(['+', 'Z']).next()?;
    let mut d = date.split('-');
    let (y, m, day): (i64, i64, i64) = (
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
    );
    let mut t = time.split(':');
    let (hh, mm, ss): (i64, i64, i64) = (
        t.next()?.parse().ok()?,
        t.next()?.parse().ok()?,
        t.next().unwrap_or("0").split('.').next()?.parse().ok()?,
    );
    // Days from civil (Howard Hinnant's algorithm), good for any Gregorian date.
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hh * 3600 + mm * 60 + ss)
}
