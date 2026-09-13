//! `{room}`/`{entity}` template expansion in the Python SDK (#68).
//!
//! Adapters build their keys themselves, so until commandable virtual
//! entities there was no automation binding entities through templates and
//! no coverage of `Context` expanding them. Both halves were broken, and
//! asymmetrically: a templated publish raised (the coverage check compared
//! the concrete key against a literal `{room}` chunk), while a templated
//! subscribe handed zenoh that chunk verbatim, matched nothing, and said
//! nothing — a unit reporting healthy and deaf to every command.
//!
//! `latches` binds two entities in two rooms through one subscribe and one
//! publish expression. Commanding either latch and seeing its state come
//! back exercises both halves over both expansions, which is what `plan`
//! prints for this fixture.
//!
//! Determinism: the command publishers wait for a matching subscriber
//! (the evening.rs pattern), which is also where a regression in the
//! subscribe half surfaces — no expansion, no match. States are read both
//! live and from the core mirror, so a publish that beats the subscriber
//! is not lost.

mod common;

use std::time::Duration;

use homeostat::bus::HealthStatus;
use serde_json::json;

use common::{await_health, await_states, health_watch, matched_publisher, Supervisor};

const FIXTURE: &str = "tests/fixture_house_templates";

#[tokio::test(flavor = "multi_thread")]
async fn templated_binding_reaches_every_bound_entity() {
    let mut sup = Supervisor::spawn(FIXTURE);
    let observer = sup.observer().await;

    let mut latches = health_watch(&observer, "latches").await;
    await_health(&mut latches, Duration::from_secs(120), |h| {
        h.status == HealthStatus::Running
    })
    .await;

    let states = observer
        .declare_subscriber("home/state/**")
        .await
        .expect("state subscriber");

    // One latch per room: `{room}` and `{entity}` both have to move.
    for key in [
        "home/cmd/global/night_mode/on",
        "home/cmd/hallway/motion_lighting/on",
    ] {
        let publisher = matched_publisher(&observer, key).await;
        publisher
            .put(
                serde_json::to_vec(&json!({
                    "value": true,
                    "priority": "family",
                    "actor": "test",
                }))
                .expect("envelope"),
            )
            .await
            .expect("command put");
    }

    await_states(
        &observer,
        &states,
        &[
            ("home/state/global/night_mode/on", json!(true)),
            ("home/state/hallway/motion_lighting/on", json!(true)),
        ],
    )
    .await;

    sup.shutdown();
}
