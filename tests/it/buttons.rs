//! Buttons service integration tests: a real supervisor on the buttons
//! fixture house, where the reflector turns each command back into state,
//! asserting on what a press sends (docs/design.md#gesture-actions).

use std::time::Duration;

use serde_json::json;

use crate::common::{
    await_mirror, eventually, expect_silence, next_sample, restart_unit, up, Msg, StateSub,
    Supervisor, SILENCE,
};

const FIXTURE: &str = "tests/fixtures/house_buttons";
const PRESS_KEY: &str = "home/state/hall/remote/action";
const LIGHT_STATE: &str = "home/state/hall/hall_light/on";
const LIGHT_CMDS: &str = "home/cmd/hall/hall_light/*";

/// The aspect a command is for, the last segment of its key.
fn aspect(cmd: &Msg) -> &str {
    cmd.key.rsplit('/').next().expect("a cmd key has segments")
}

async fn press(session: &zenoh::Session, gesture: &str) {
    session
        .put(PRESS_KEY, json!(gesture).to_string())
        .await
        .expect("press put");
}

/// Presses `gesture` until the light gets a command for `value`, and
/// returns that command. The service reads the light's state off its own
/// subscription, which can lag the mirror. A press it handles first finds
/// the state unknown and sends nothing, or finds the old state and sends
/// the old toggle, which the reflector turns back into the same state.
async fn press_until(session: &zenoh::Session, cmds: &StateSub, gesture: &str, value: bool) -> Msg {
    let what = format!("command for {value} on a {gesture} press");
    eventually(Duration::from_secs(20), &what, || async {
        press(session, gesture).await;
        // Long enough that a slow command is not left queued behind the
        // next press, where a later read would take it.
        let cmd = next_sample(cmds, Duration::from_secs(5)).await?;
        (cmd.value()["value"] == json!(value)).then_some(cmd)
    })
    .await
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
    await_mirror(&observer, LIGHT_STATE, &json!(false)).await;
    let cmd = press_until(&observer, &cmds, "single", true).await;
    assert_eq!(aspect(&cmd), "on");
    let envelope = cmd.value();
    assert_eq!(
        (
            &envelope["value"],
            &envelope["priority"],
            &envelope["actor"]
        ),
        (&json!(true), &json!("manual"), &json!("buttons"))
    );
    assert!(envelope.get("hold").is_none(), "no hold named, none sent");

    // The reflector turns it into state, so the next one turns it off.
    await_mirror(&observer, LIGHT_STATE, &json!(true)).await;
    press_until(&observer, &cmds, "single", false).await;

    // A set sends one command per aspect.
    press(&observer, "double").await;
    let mut sent = Vec::new();
    for _ in 0..2 {
        let cmd = next_sample(&cmds, Duration::from_secs(10))
            .await
            .expect("a command per aspect");
        sent.push((aspect(&cmd).to_string(), cmd.value()["value"].clone()));
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
    expect_silence(&cmds, SILENCE, "command for an unmapped gesture").await;

    // A restart does not act on the last press, which the mirror still holds.
    press(&observer, "single").await;
    assert!(next_sample(&cmds, Duration::from_secs(10)).await.is_some());
    restart_unit(&observer, "buttons").await;
    expect_silence(&cmds, SILENCE, "command from a replayed press").await;

    sup.shutdown();
}
