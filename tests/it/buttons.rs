//! Buttons service integration tests: a real supervisor on the buttons
//! fixture house, where the reflector turns each command back into state,
//! asserting on what a press sends (docs/design.md#gesture-actions).

use std::time::Duration;

use serde_json::{json, Value};
use zenoh::handlers::FifoChannelHandler;
use zenoh::pubsub::Subscriber;

use crate::common::{restart_unit, up, Supervisor};

const FIXTURE: &str = "tests/fixtures/house_buttons";
const PRESS_KEY: &str = "home/state/hall/remote/action";
const LIGHT_STATE: &str = "home/state/hall/hall_light/on";
const LIGHT_CMDS: &str = "home/cmd/hall/hall_light/*";

type Sub = Subscriber<FifoChannelHandler<zenoh::sample::Sample>>;

/// The next command within the timeout, as (aspect, envelope).
async fn next_cmd(sub: &Sub, timeout: Duration) -> Option<(String, Value)> {
    let sample = tokio::time::timeout(timeout, sub.recv_async())
        .await
        .ok()?
        .ok()?;
    let aspect = sample.key_expr().as_str().rsplit('/').next()?.to_string();
    Some((
        aspect,
        serde_json::from_slice(&sample.payload().to_bytes()).ok()?,
    ))
}

async fn press(session: &zenoh::Session, gesture: &str) {
    session
        .put(PRESS_KEY, json!(gesture).to_string())
        .await
        .expect("press put");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_press_sends_the_commands_its_gesture_names_and_only_those() {
    let (mut sup, observer) = up(|| Supervisor::spawn(FIXTURE), &["buttons"]).await;
    let cmds = observer
        .declare_subscriber(LIGHT_CMDS)
        .await
        .expect("cmd subscriber");

    // The light reports off, so a single press turns it on, at the manual
    // band, from the service.
    observer.put(LIGHT_STATE, "false").await.expect("state put");
    tokio::time::sleep(Duration::from_millis(300)).await;
    press(&observer, "single").await;
    let (aspect, envelope) = next_cmd(&cmds, Duration::from_secs(10))
        .await
        .expect("a command for the single press");
    assert_eq!(aspect, "on");
    assert_eq!(
        (
            &envelope["value"],
            &envelope["priority"],
            &envelope["actor"]
        ),
        (&json!(true), &json!("manual"), &json!("buttons"))
    );
    assert!(envelope.get("hold").is_none(), "no hold named, none sent");

    // The reflector turned it into state, so the next one turns it off.
    tokio::time::sleep(Duration::from_millis(300)).await;
    press(&observer, "single").await;
    let (_, envelope) = next_cmd(&cmds, Duration::from_secs(10))
        .await
        .expect("a command for the second press");
    assert_eq!(envelope["value"], json!(false), "a toggle flips the state");

    // A set sends one command per aspect.
    press(&observer, "double").await;
    let mut sent = Vec::new();
    for _ in 0..2 {
        let (aspect, envelope) = next_cmd(&cmds, Duration::from_secs(10))
            .await
            .expect("a command per aspect");
        sent.push((aspect, envelope["value"].clone()));
    }
    sent.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        sent,
        [
            ("brightness".to_string(), json!(120)),
            ("on".to_string(), json!(true))
        ]
    );

    // An unmapped gesture sends nothing.
    press(&observer, "triple").await;
    assert!(
        next_cmd(&cmds, Duration::from_secs(1)).await.is_none(),
        "an unmapped gesture sends nothing"
    );

    // A restart does not act on the last press, which the mirror still holds.
    press(&observer, "single").await;
    assert!(next_cmd(&cmds, Duration::from_secs(10)).await.is_some());
    restart_unit(&observer, "buttons").await;
    assert!(
        next_cmd(&cmds, Duration::from_secs(2)).await.is_none(),
        "a restart replays no press"
    );

    sup.shutdown();
}
