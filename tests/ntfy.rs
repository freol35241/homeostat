//! ntfy notifier adapter integration tests: each scenario spawns a fake
//! ntfy server (tests/fake_ntfy.py — the real publish shape, bearer-token
//! checking, a controllable outage) on a free port plus the real
//! supervisor on the ntfy fixture house, and asserts on the bus.

mod common;

use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use homeostat::bus::HealthStatus;
use serde_json::{json, Value};

use common::{
    assert_cli_ok, assert_unit_contract, await_health, await_mirror, await_states, cli,
    expect_drop_event, free_port, health_watch, matched_publisher, stdout, temp_house,
    StateSub, Supervisor,
};

const FIXTURE: &str = "tests/fixture_house_ntfy";
const PORT_ENV: &str = "HOMEOSTAT_TEST_NTFY_PORT";
const TOKEN_ENV: &str = "HOMEOSTAT_NTFY_TOKEN";
const TOKEN: &str = "tk_test_publisher";
const EVENT_KEY: &str = "home/health/ntfy/event";
const ALICE_ALERT: &str = "home/cmd/person/alice_phone/alert";
const ALICE_MESSAGE: &str = "home/cmd/person/alice_phone/message";
const ADULTS_MESSAGE: &str = "home/cmd/global/adults/message";
const ALICE_AVAILABLE: &str = "home/state/person/alice_phone/available";
const ALICE_DELIVERED: &str = "home/state/person/alice_phone/delivered";

/// A fake ntfy server on a free port, killed on drop.
struct FakeNtfy {
    child: Child,
    port: u16,
}

impl FakeNtfy {
    fn spawn() -> Self {
        let port = free_port();
        let child = Command::new("uv")
            .args(["run", "tests/fake_ntfy.py", "--port", &port.to_string(), "--token", TOKEN])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn fake ntfy (is uv installed?)");
        let deadline = Instant::now() + Duration::from_secs(60);
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(Instant::now() < deadline, "fake ntfy never listened on {port}");
            std::thread::sleep(Duration::from_millis(50));
        }
        Self { child, port }
    }

    fn http(&self, method: &str, path: &str) -> String {
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", self.port))
            .expect("connect to fake ntfy");
        stream
            .write_all(
                format!(
                    "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .expect("write request");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("read response");
        assert!(response.starts_with("HTTP/1.0 200") || response.starts_with("HTTP/1.1 200"), "{method} {path}: {response}");
        response.rsplit("\r\n\r\n").next().expect("body").to_string()
    }

    fn control(&self, path: &str) {
        self.http("POST", path);
    }

    /// Every publish the server accepted so far.
    fn received(&self) -> Vec<Value> {
        serde_json::from_str(&self.http("GET", "/control/received")).expect("received is JSON")
    }

    /// Polls until the server has accepted `n` publishes.
    fn await_received(&self, n: usize) -> Vec<Value> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let received = self.received();
            if received.len() >= n {
                return received;
            }
            assert!(Instant::now() < deadline, "only {} publishes reached the server", received.len());
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for FakeNtfy {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn wish(text: Value, actor: &str) -> Value {
    json!({"value": text, "priority": "automation", "actor": actor})
}

async fn setup() -> (FakeNtfy, Supervisor, zenoh::Session) {
    let server = FakeNtfy::spawn();
    let port = server.port.to_string();
    let sup = Supervisor::spawn_with_env(FIXTURE, &[(PORT_ENV, &port), (TOKEN_ENV, TOKEN)]);
    let observer = sup.observer().await;
    let mut watch = health_watch(&observer, "ntfy").await;
    await_health(&mut watch, Duration::from_secs(120), |h| h.status == HealthStatus::Running)
        .await;
    (server, sup, observer)
}

async fn event_sub(observer: &zenoh::Session) -> StateSub {
    observer.declare_subscriber(EVENT_KEY).await.expect("event subscriber")
}

/// (a) An alert on a person's channel reaches the server on the topic the
/// entity id names, at ntfy's max priority, titled with the actor; a
/// message on the group topic goes at the default priority; the server's
/// time publishes as `delivered` and the channel is available. Then the
/// unit contract.
#[tokio::test(flavor = "multi_thread")]
async fn delivers_by_topic_with_severity_as_priority() {
    let (server, mut sup, observer) = setup().await;
    let state_sub = observer
        .declare_subscriber("home/state/*/*/*")
        .await
        .expect("state subscriber");

    let alert = matched_publisher(&observer, ALICE_ALERT).await;
    alert.put(wish(json!("Motion in the hall and nobody home"), "intrusion").to_string()).await.expect("put");
    let received = server.await_received(1);
    assert_eq!(received[0]["topic"], "alice");
    assert_eq!(received[0]["message"], "Motion in the hall and nobody home");
    assert_eq!(received[0]["priority"], 5);
    assert_eq!(received[0]["title"], "intrusion");
    let delivered_at = received[0]["time"].clone();

    let message = matched_publisher(&observer, ADULTS_MESSAGE).await;
    message.put(wish(json!("Irrigation skipped: it rained yesterday"), "irrigation").to_string()).await.expect("put");
    let received = server.await_received(2);
    assert_eq!(received[1]["topic"], "adults");
    assert_eq!(received[1]["priority"], 3);

    await_states(
        &observer,
        &state_sub,
        &[
            (ALICE_AVAILABLE, json!(true)),
            (ALICE_DELIVERED, delivered_at),
            ("home/state/global/adults/available", json!(true)),
        ],
    )
    .await;

    assert_unit_contract(&mut sup, &observer, "ntfy").await;
}

/// (b) What never reaches the server: a non-string value, an empty
/// string, an unknown aspect, an envelope-less payload — each an
/// invalid-command drop — and a second message inside the floor, a
/// rate-limited drop.
#[tokio::test(flavor = "multi_thread")]
async fn invalid_and_flooding_commands_drop() {
    let (server, _sup, observer) = setup().await;
    let events = event_sub(&observer).await;

    let alert = matched_publisher(&observer, ALICE_ALERT).await;
    alert.put(wish(json!(42), "x").to_string()).await.expect("put");
    expect_drop_event(&events, "invalid-command").await;
    alert.put(wish(json!("   "), "x").to_string()).await.expect("put");
    expect_drop_event(&events, "invalid-command").await;
    alert.put(json!("bare string, no envelope").to_string()).await.expect("put");
    expect_drop_event(&events, "invalid-command").await;

    let odd = matched_publisher(&observer, "home/cmd/person/alice_phone/ring").await;
    odd.put(wish(json!("hello"), "x").to_string()).await.expect("put");
    let event = expect_drop_event(&events, "invalid-command").await;
    assert_eq!(event["aspect"], "ring");

    let message = matched_publisher(&observer, ALICE_MESSAGE).await;
    message.put(wish(json!("first"), "x").to_string()).await.expect("put");
    message.put(wish(json!("second, inside the floor"), "x").to_string()).await.expect("put");
    let event = expect_drop_event(&events, "rate-limited").await;
    assert_eq!(event["key"], ALICE_MESSAGE);
    let received = server.await_received(1);
    assert_eq!(received.len(), 1, "only the first message was sent: {received:?}");
    assert_eq!(received[0]["message"], "first");
}

/// (c) A server that stops accepting publishes: the message drops with
/// delivery-failed, the channel goes unavailable; the first success after
/// recovery brings it back and publishes a fresh `delivered`.
#[tokio::test(flavor = "multi_thread")]
async fn failed_delivery_is_loud_and_recovers() {
    let (server, _sup, observer) = setup().await;
    let events = event_sub(&observer).await;
    let state_sub = observer
        .declare_subscriber("home/state/person/alice_phone/*")
        .await
        .expect("state subscriber");
    let alert = matched_publisher(&observer, ALICE_ALERT).await;

    alert.put(wish(json!("before the outage"), "x").to_string()).await.expect("put");
    server.await_received(1);
    await_mirror(&observer, ALICE_AVAILABLE, &json!(true)).await;

    server.control("/control/break");
    tokio::time::sleep(Duration::from_millis(600)).await; // past the floor
    alert.put(wish(json!("lost"), "x").to_string()).await.expect("put");
    let event = expect_drop_event(&events, "delivery-failed").await;
    assert_eq!(event["key"], ALICE_ALERT);
    assert!(event["error"].as_str().unwrap_or("").contains("500"), "{event}");
    await_mirror(&observer, ALICE_AVAILABLE, &json!(false)).await;

    server.control("/control/restore");
    tokio::time::sleep(Duration::from_millis(600)).await;
    alert.put(wish(json!("after"), "x").to_string()).await.expect("put");
    let received = server.await_received(2);
    assert_eq!(received[1]["message"], "after");
    await_states(
        &observer,
        &state_sub,
        &[(ALICE_AVAILABLE, json!(true)), (ALICE_DELIVERED, received[1]["time"].clone())],
    )
    .await;
}

/// (d) A wrong token is refused by the server at the first publish, not
/// at startup (ntfy's health endpoint is unauthenticated): the message
/// drops with delivery-failed naming the 401. An unset token, and an
/// unreachable server, never reach `running` at all.
#[tokio::test(flavor = "multi_thread")]
async fn bad_credentials_and_dead_server_are_visible() {
    let server = FakeNtfy::spawn();
    let port = server.port.to_string();
    let sup = Supervisor::spawn_with_env(FIXTURE, &[(PORT_ENV, &port), (TOKEN_ENV, "tk_wrong")]);
    let observer = sup.observer().await;
    let mut watch = health_watch(&observer, "ntfy").await;
    await_health(&mut watch, Duration::from_secs(120), |h| h.status == HealthStatus::Running)
        .await;
    let events = event_sub(&observer).await;
    let alert = matched_publisher(&observer, ALICE_ALERT).await;
    alert.put(wish(json!("hello"), "x").to_string()).await.expect("put");
    let event = expect_drop_event(&events, "delivery-failed").await;
    assert!(event["error"].as_str().unwrap_or("").contains("401"), "{event}");
    await_mirror(&observer, ALICE_AVAILABLE, &json!(false)).await;
    drop(sup);

    // No token: a startup error, so the unit cycles through backoff.
    let sup = Supervisor::spawn_with_env(FIXTURE, &[(PORT_ENV, &port)]);
    let observer = sup.observer().await;
    let mut watch = health_watch(&observer, "ntfy").await;
    await_health(&mut watch, Duration::from_secs(120), |h| h.status == HealthStatus::Backoff)
        .await;
    drop(sup);

    // No server at all: the health check fails before ready().
    let dead = free_port().to_string();
    let sup = Supervisor::spawn_with_env(FIXTURE, &[(PORT_ENV, &dead), (TOKEN_ENV, TOKEN)]);
    let observer = sup.observer().await;
    let mut watch = health_watch(&observer, "ntfy").await;
    await_health(&mut watch, Duration::from_secs(120), |h| h.status == HealthStatus::Backoff)
        .await;
}

/// (e) The gate: granting an automation a notifier publish is a grant
/// delta, so the plan is structural and renders who may reach whom.
#[test]
fn granting_a_notifier_publish_is_structural() {
    let house = temp_house(FIXTURE, "ntfy-grant");
    std::fs::write(
        house.join("units/intrusion.toml"),
        r#"schema = 1

[unit]
name = "intrusion"
kind = "automation"

[runtime]
command = "uv run units/intrusion.py"
restart = "on-failure"

[bus.subscribes]
motion = "home/state/*/*/occupancy"

[bus.publishes]
alert = { key = "home/cmd/person/*/alert", capability = "notifier", priority = "automation" }
"#,
    )
    .expect("write manifest");
    std::fs::write(house.join("units/intrusion.py"), "").expect("write script");

    let plan = cli(&["plan", house.to_str().expect("utf-8 path")]);
    assert_cli_ok(&plan);
    let text = stdout(&plan);
    assert!(text.contains("Plan tier: structural"), "{text}");
    assert!(
        text.contains("intrusion.alert  capability=notifier  priority=automation"),
        "the grant is rendered: {text}"
    );
    assert!(text.contains("-> alice_phone"), "{text}");
    // The alert grant's own entry (not the plan as a whole — ntfy's own
    // adapter-binding entry legitimately lists every entity it owns,
    // "adults" included): every grant entry starts at 2-space indent, its
    // detail lines are indented further, so the next 2-space-indent line
    // ends it.
    let alert_grant: String = text
        .lines()
        .skip_while(|l| !l.starts_with("  intrusion.alert"))
        .skip(1)
        .take_while(|l| l.starts_with("    "))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !alert_grant.contains("-> adults"),
        "the alert grant covers the person channel only: {alert_grant}"
    );
}
