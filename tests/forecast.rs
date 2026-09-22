//! Forecast bus class, end to end (docs/design.md, Forecasts).
//!
//! 1. A unit may publish `home/forecast/{room}/{entity}/{aspect}/{source}`
//!    for an entity that EXISTS — it need not bind it — and the plan
//!    accepts the class and the key shape.
//! 2. The payload is the SDK's: `schema`, `issued`, and irregular `points`
//!    ascending in time, each declaring the window it covers. Irregular is
//!    the point — a regular grid could not carry the hourly-then-coarser
//!    shape real sources publish, nor say how long a value holds.
//! 3. The core MIRRORS the class, which is what makes a forecast usable at
//!    all: a day-ahead curve is published once a day, so a consumer that
//!    starts after the publish must still get it. This is the property the
//!    whole class rests on, so it is read here through a session that was
//!    not connected when the unit published.

mod common;

use std::time::Duration;

use homeostat::bus::HealthStatus;
use serde_json::Value;

use common::{await_health, health_watch, matched_publisher, Supervisor};

const FIXTURE: &str = "tests/fixture_house_forecast";
const FORECAST_KEY: &str = "home/forecast/global/spot_price/price/nordpool";

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

    // Each point says what it covers, so a reader never has to infer a
    // hold length from the gap to the next point — which the last point,
    // having no next, could not do at all.
    let extents: Vec<f64> = points
        .iter()
        .map(|p| p["d"].as_f64().expect("point extent"))
        .collect();
    assert_eq!(
        extents,
        vec![3600.0, 3600.0, 3600.0, 10800.0, 10800.0, 10800.0],
        "every point declares its window: {payload}"
    );

    sup.shutdown();
}

/// Epoch seconds from an RFC3339 instant with a UTC offset, without adding
/// a date dependency for one assertion: the fixture publishes `Z`-less
/// ISO with a `+00:00` offset, which is all this needs to handle.
/// The mirror is in memory, so a core restart empties it — and a forecast
/// is then only as available as its producer's startup behaviour. A
/// producer that issues on a schedule and not at start leaves the class
/// answering nothing, which for a once-daily curve is most of a day
/// (docs/design.md, Forecasts: "a producer should publish its current
/// forecast at startup").
///
/// Published from the test session rather than from `seer`, because
/// `seer` re-issues at start and would mask exactly the gap this pins.
/// The convention exists because of this behaviour; the test is what
/// makes the reason executable rather than a sentence in a document.
#[tokio::test(flavor = "multi_thread")]
async fn a_core_restart_empties_the_mirror_and_only_a_producer_refills_it() {
    const KEY: &str = "home/forecast/global/spot_price/price/oracle";
    let issue = serde_json::json!({
        "schema": 1,
        "issued": "2020-01-01T08:00:00+00:00",
        "points": [{"t": "2020-01-01T12:00:00+00:00", "v": 3.5, "d": 3600.0}],
    });

    let mut sup = Supervisor::spawn(FIXTURE);
    {
        let observer = sup.observer().await;
        let publisher = matched_publisher(&observer, KEY).await;
        publisher
            .put(issue.to_string())
            .await
            .expect("publish forecast");
        let seen = mirror_read_eventually(&observer, KEY).await;
        assert_eq!(seen["points"][0]["v"], serde_json::json!(3.5), "{seen}");
    }
    sup.shutdown();

    // A new core, the same house, nothing republishing this key.
    let mut restarted = Supervisor::spawn(FIXTURE);
    let observer = restarted.observer().await;
    let mut seer = health_watch(&observer, "seer").await;
    await_health(&mut seer, Duration::from_secs(120), |h| {
        h.status == HealthStatus::Running
    })
    .await;
    // `seer` is up and has re-issued its OWN key, so the mirror is
    // serving forecasts again — this one is simply not among them.
    mirror_read_eventually(&observer, FORECAST_KEY).await;
    assert!(
        query_once(&observer, KEY).await.is_none(),
        "a forecast nobody republished must not survive the core that held it"
    );

    // And it comes back the only way it can: someone says it again.
    let publisher = matched_publisher(&observer, KEY).await;
    publisher
        .put(issue.to_string())
        .await
        .expect("republish forecast");
    let again = mirror_read_eventually(&observer, KEY).await;
    assert_eq!(again["points"][0]["v"], serde_json::json!(3.5), "{again}");

    restarted.shutdown();
}

/// One query, one pass: whether the mirror holds this key right now.
async fn query_once(session: &zenoh::Session, key: &str) -> Option<Value> {
    let replies = session.get(key).await.expect("mirror query");
    while let Ok(reply) = replies.recv_async().await {
        if let Ok(sample) = reply.result() {
            return serde_json::from_slice(&sample.payload().to_bytes()).ok();
        }
    }
    None
}

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
