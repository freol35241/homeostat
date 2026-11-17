//! `{room}`/`{entity}` template expansion in the Python SDK.
//!
//! Adapters build their keys themselves, so only an automation binding
//! entities through templates exercises `Context` expanding them. The two
//! halves fail asymmetrically: an unexpanded templated publish raises (the
//! coverage check compares the concrete key against a literal `{room}`
//! chunk). An unexpanded templated subscribe hands zenoh that chunk
//! verbatim, matches nothing and reports nothing, which leaves a unit
//! that reports healthy and is deaf to every command.
//!
//! `latches` binds two entities in two rooms through one subscribe and one
//! publish expression. Commanding either latch and seeing its state come
//! back exercises both halves over both expansions, which is what `plan`
//! prints for this fixture.
//!
//! Determinism: the command publishers wait for a matching subscriber
//! (the evening.rs pattern), which is also where a regression in the
//! subscribe half surfaces: without expansion there is no match. States
//! are read both live and from the core mirror, so a publish that beats
//! the subscriber is not lost.

use serde_json::json;

use crate::common::{await_states, matched_publisher, up, Supervisor};

const FIXTURE: &str = "tests/fixtures/house_templates";

#[tokio::test(flavor = "multi_thread")]
async fn templated_binding_reaches_every_bound_entity() {
    let (mut sup, observer) = up(|| Supervisor::spawn(FIXTURE), &["latches"]).await;

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
