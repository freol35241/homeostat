//! Forecast bus class, end to end (docs/design.md#forecasts).
//!
//! 1. A unit may publish `home/forecast/{room}/{entity}/{aspect}/{source}`
//!    for an entity that exists, without binding it, and the plan accepts
//!    the class and the key shape.
//! 2. The payload is the SDK's: `schema`, `issued`, and irregular `points`
//!    ascending in time, each declaring the window it covers. The points
//!    are irregular because a regular grid could not carry the
//!    hourly-then-coarser shape real sources publish, nor say how long a
//!    value holds.
//! 3. The core mirrors the class, which is what makes a forecast usable:
//!    a day-ahead curve is published once a day, so a consumer that starts
//!    after the publish must still get it. The class depends on this, so it
//!    is read here through a session that was not connected when the unit
//!    published.

use std::time::Duration;

use crate::civil::utc_epoch_seconds;
use crate::common::{cache_read, eventually, matched_publisher, up, Supervisor};

const FIXTURE: &str = "tests/fixtures/house_forecast";
const FORECAST_KEY: &str = "home/forecast/global/spot_price/price/nordpool";

#[tokio::test(flavor = "multi_thread")]
async fn a_forecast_is_published_and_mirrored_for_a_late_joiner() {
    let (mut sup, _observer) = up(|| Supervisor::spawn(FIXTURE), &["seer"]).await;

    // A session opened after the unit published: the only way it can see
    // the forecast is the core's mirror. A day-ahead curve arrives once a
    // day, so without this a consumer restarting mid-horizon would run
    // blind until the next issue.
    let late_joiner = sup.observer().await;
    let payload = eventually(Duration::from_secs(15), "mirrored forecast", || {
        cache_read(&late_joiner, FORECAST_KEY)
    })
    .await;

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
        .map(|t| utc_epoch_seconds(t).expect("point time parses as RFC3339 with an offset"))
        .collect();
    let gaps: Vec<i64> = parsed.windows(2).map(|w| w[1] - w[0]).collect();
    assert_eq!(
        gaps,
        vec![3600, 3600, 3600, 10800, 10800],
        "the horizon is irregular by design: {payload}"
    );

    assert_eq!(points[0]["v"], 1.20, "{payload}");

    // Each point says what it covers, so a reader does not have to infer a
    // hold length from the gap to the next point. The last point has no
    // next point, so that inference would fail for it.
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

/// The mirror is in memory, so a core restart empties it, and a forecast
/// is then available again only if its producer publishes at startup. A
/// producer that issues on a schedule and not at start leaves the class
/// answering nothing, which for a once-daily curve is most of a day
/// (docs/design.md#forecasts: "Producers publish their current forecast
/// at startup").
///
/// Published from the test session rather than from `seer`, because
/// `seer` re-issues at start and would hide the gap this test pins. The
/// startup convention exists because of this behaviour.
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
        let seen = eventually(Duration::from_secs(15), "mirrored forecast", || {
            cache_read(&observer, KEY)
        })
        .await;
        assert_eq!(seen["points"][0]["v"], serde_json::json!(3.5), "{seen}");
    }
    sup.shutdown();

    // A new core, the same house, nothing republishing this key.
    let (mut restarted, observer) = up(|| Supervisor::spawn(FIXTURE), &["seer"]).await;
    // `seer` is up and has re-issued its own key, so the mirror is
    // serving forecasts again. This one is not among them.
    eventually(Duration::from_secs(15), "mirrored forecast", || {
        cache_read(&observer, FORECAST_KEY)
    })
    .await;
    assert!(
        cache_read(&observer, KEY).await.is_none(),
        "a forecast nobody republished must not survive the core that held it"
    );

    // It comes back only when a producer publishes it again.
    let publisher = matched_publisher(&observer, KEY).await;
    publisher
        .put(issue.to_string())
        .await
        .expect("republish forecast");
    let again = eventually(Duration::from_secs(15), "mirrored forecast", || {
        cache_read(&observer, KEY)
    })
    .await;
    assert_eq!(again["points"][0]["v"], serde_json::json!(3.5), "{again}");

    restarted.shutdown();
}
