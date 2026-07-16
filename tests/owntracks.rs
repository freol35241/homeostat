//! OwnTracks adapter integration tests: each scenario spawns a real
//! mosquitto broker on a free port plus the real supervisor on the
//! owntracks fixture house, and asserts on both buses.

mod common;

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use homeostat::bus::HealthStatus;
use rumqttc::{AsyncClient, Event, Incoming, MqttOptions, QoS};
use serde_json::{json, Value};
use zenoh::handlers::FifoChannelHandler;
use zenoh::pubsub::Subscriber;
use zenoh::sample::SampleKind;

use common::{await_health, free_port, health_watch, process_alive, Supervisor};

const FIXTURE: &str = "tests/fixture_house_owntracks";
const PORT_ENV: &str = "HOMEOSTAT_TEST_MQTT_PORT";
const EVENT_KEY: &str = "home/health/owntracks/event";

/// A mosquitto broker on a free port, killed on drop.
struct Mosquitto {
    child: Child,
    port: u16,
    conf: PathBuf,
}

impl Mosquitto {
    fn spawn() -> Self {
        let port = free_port();
        let conf = std::env::temp_dir().join(format!("homeostat-owntracks-{port}.conf"));
        std::fs::write(&conf, format!("listener {port} 127.0.0.1\nallow_anonymous true\n"))
            .expect("write mosquitto config");
        // Debian puts mosquitto in /usr/sbin, which is not always on PATH.
        let child = ["mosquitto", "/usr/sbin/mosquitto"]
            .iter()
            .find_map(|bin| {
                Command::new(bin)
                    .args(["-c", conf.to_str().expect("utf-8 conf path")])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .ok()
            })
            .expect("spawn mosquitto (is it installed?)");
        let deadline = Instant::now() + Duration::from_secs(10);
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(Instant::now() < deadline, "mosquitto never listened on {port}");
            std::thread::sleep(Duration::from_millis(50));
        }
        Self { child, port, conf }
    }
}

impl Drop for Mosquitto {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.conf);
    }
}

/// An MQTT test client: publishes are acknowledged (QoS 1) before returning.
struct Mqtt {
    client: AsyncClient,
    events: tokio::sync::mpsc::UnboundedReceiver<Event>,
}

impl Mqtt {
    async fn connect(port: u16, id: &str) -> Self {
        let mut opts = MqttOptions::new(id, "127.0.0.1", port);
        opts.set_keep_alive(Duration::from_secs(5));
        let (client, mut eventloop) = AsyncClient::new(opts, 64);
        let (tx, events) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok(event) = eventloop.poll().await {
                if tx.send(event).is_err() {
                    break;
                }
            }
        });
        let mut mqtt = Self { client, events };
        mqtt.await_event(|i| matches!(i, Incoming::ConnAck(_))).await;
        mqtt
    }

    async fn publish(&mut self, topic: &str, payload: &str) {
        self.client
            .publish(topic, QoS::AtLeastOnce, false, payload)
            .await
            .expect("mqtt publish");
        self.await_event(|i| matches!(i, Incoming::PubAck(_))).await;
    }

    async fn await_event<F: Fn(&Incoming) -> bool>(&mut self, pred: F) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let event = tokio::time::timeout_at(deadline, self.events.recv())
                .await
                .expect("mqtt event within 10s")
                .expect("mqtt event loop alive");
            if let Event::Incoming(incoming) = event {
                if pred(&incoming) {
                    return;
                }
            }
        }
    }
}

/// Spawns broker + supervisor on the fixture and waits for the adapter's
/// liveliness token (generous timeout: first run resolves the uv env).
async fn setup() -> (Mosquitto, Supervisor, zenoh::Session) {
    let mosquitto = Mosquitto::spawn();
    let port = mosquitto.port.to_string();
    let sup = Supervisor::spawn_with_env(FIXTURE, &[(PORT_ENV, &port)]);
    let observer = sup.observer().await;
    let token_sub = observer
        .liveliness()
        .declare_subscriber("home/health/owntracks/alive")
        .history(true)
        .await
        .expect("liveliness subscriber");
    let token = tokio::time::timeout(Duration::from_secs(60), token_sub.recv_async())
        .await
        .expect("adapter liveliness token within 60s")
        .expect("liveliness stream open");
    assert_eq!(token.kind(), SampleKind::Put);
    (mosquitto, sup, observer)
}

type StateSub = Subscriber<FifoChannelHandler<zenoh::sample::Sample>>;

/// Collects state samples until every `expected` (key, value) has appeared.
async fn expect_states(sub: &StateSub, expected: &[(&str, Value)]) {
    let mut seen: HashMap<String, Value> = HashMap::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while expected
        .iter()
        .any(|(key, value)| seen.get(*key) != Some(value))
    {
        let sample = tokio::time::timeout_at(deadline, sub.recv_async())
            .await
            .unwrap_or_else(|_| panic!("missing state keys; saw {seen:?}"))
            .expect("state stream open");
        let value: Value = serde_json::from_slice(&sample.payload().to_bytes())
            .expect("state payload is JSON");
        seen.insert(sample.key_expr().as_str().to_string(), value);
    }
}

/// Reads health events until one matches the expected drop reason.
async fn expect_drop_event(sub: &StateSub, reason: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let sample = tokio::time::timeout_at(deadline, sub.recv_async())
            .await
            .unwrap_or_else(|_| panic!("no \"{reason}\" health event within 10s"))
            .expect("event stream open");
        let event: Value = serde_json::from_slice(&sample.payload().to_bytes())
            .expect("health event is JSON");
        assert_eq!(event["kind"], "drop", "unexpected event kind: {event}");
        if event["reason"] == reason {
            return;
        }
    }
}

/// (a) A location fix translates to the scalar per-aspect state keys under
/// the reserved "person" pseudo-room.
#[tokio::test(flavor = "multi_thread")]
async fn location_translates_to_bus_state() {
    let (mosquitto, mut sup, observer) = setup().await;
    let state_sub = observer
        .declare_subscriber("home/state/**")
        .await
        .expect("state subscriber");
    let mut mqtt = Mqtt::connect(mosquitto.port, "test-state").await;

    mqtt.publish(
        "owntracks/alice/phone",
        r#"{"_type":"location","lat":59.33,"lon":18.06,"acc":12,"batt":87,"tst":1752600000}"#,
    )
    .await;
    expect_states(
        &state_sub,
        &[
            ("home/state/person/alice_phone/lat", json!(59.33)),
            ("home/state/person/alice_phone/lon", json!(18.06)),
            ("home/state/person/alice_phone/accuracy", json!(12)),
            ("home/state/person/alice_phone/battery", json!(87)),
            ("home/state/person/alice_phone/fixed_at", json!(1752600000)),
        ],
    )
    .await;

    sup.shutdown();
}

/// (b) Non-location `_type` payloads (transition, lwt, waypoint, ...) are
/// normal OwnTracks traffic: ignored silently, no health event, no state.
#[tokio::test(flavor = "multi_thread")]
async fn non_location_type_is_ignored() {
    let (mosquitto, mut sup, observer) = setup().await;
    let event_sub = observer
        .declare_subscriber(EVENT_KEY)
        .await
        .expect("event subscriber");
    let state_sub = observer
        .declare_subscriber("home/state/**")
        .await
        .expect("state subscriber");
    let mut mqtt = Mqtt::connect(mosquitto.port, "test-nonloc").await;

    mqtt.publish(
        "owntracks/alice/phone",
        r#"{"_type":"transition","event":"enter","desc":"home"}"#,
    )
    .await;

    // No event, no state within a generous window — just quiet.
    let no_event = tokio::time::timeout(Duration::from_millis(1500), event_sub.recv_async()).await;
    assert!(no_event.is_err(), "transition payload must not raise a health event");
    let no_state = tokio::time::timeout(Duration::from_millis(500), state_sub.recv_async()).await;
    assert!(no_state.is_err(), "transition payload must not publish state");

    // Still translating afterwards.
    mqtt.publish(
        "owntracks/alice/phone",
        r#"{"_type":"location","lat":1.0,"lon":2.0}"#,
    )
    .await;
    expect_states(
        &state_sub,
        &[
            ("home/state/person/alice_phone/lat", json!(1.0)),
            ("home/state/person/alice_phone/lon", json!(2.0)),
        ],
    )
    .await;

    sup.shutdown();
}

/// (c) Malformed JSON, a location fix missing lat/lon, and an unbound
/// device all drop with a health event without crashing the adapter.
#[tokio::test(flavor = "multi_thread")]
async fn bad_input_drops_with_health_event() {
    let (mosquitto, mut sup, observer) = setup().await;
    let event_sub = observer
        .declare_subscriber(EVENT_KEY)
        .await
        .expect("event subscriber");
    let state_sub = observer
        .declare_subscriber("home/state/**")
        .await
        .expect("state subscriber");
    let mut mqtt = Mqtt::connect(mosquitto.port, "test-bad").await;

    mqtt.publish("owntracks/alice/phone", "certainly not json").await;
    expect_drop_event(&event_sub, "malformed-payload").await;

    mqtt.publish("owntracks/alice/phone", r#"{"_type":"location","lat":1.0}"#).await;
    expect_drop_event(&event_sub, "malformed-payload").await;

    mqtt.publish(
        "owntracks/bob/phone",
        r#"{"_type":"location","lat":3.0,"lon":4.0}"#,
    )
    .await;
    expect_drop_event(&event_sub, "unknown-device").await;

    // Still alive and translating.
    mqtt.publish(
        "owntracks/alice/phone",
        r#"{"_type":"location","lat":5.0,"lon":6.0}"#,
    )
    .await;
    expect_states(
        &state_sub,
        &[
            ("home/state/person/alice_phone/lat", json!(5.0)),
            ("home/state/person/alice_phone/lon", json!(6.0)),
        ],
    )
    .await;

    sup.shutdown();
}

/// (d) The adapter honors the step-2 unit contract: liveliness token when
/// ready, clean SIGTERM shutdown within the grace, no orphans.
#[tokio::test(flavor = "multi_thread")]
async fn adapter_honors_unit_contract() {
    let (_mosquitto, mut sup, observer) = setup().await;
    let mut watch = health_watch(&observer, "owntracks").await;
    let health = await_health(&mut watch, Duration::from_secs(10), |h| {
        h.status == HealthStatus::Running
    })
    .await;
    let adapter_pid = health.pid.expect("running unit has a pid");
    assert!(process_alive(adapter_pid), "adapter alive before shutdown");

    sup.signal(libc::SIGTERM);
    // shutdown_grace_s = 5 in the fixture; a graceful exit must fit inside
    // it with margin only for reaping and bus teardown.
    let code = sup.wait_exit(Duration::from_secs(7));
    assert_eq!(code, Some(0), "supervisor exit code");
    assert!(!process_alive(adapter_pid), "adapter must not outlive the supervisor");
}

/// (e) Discovery: every user/device pair seen on the broker — bound or
/// not — is tracked incrementally and republished at
/// home/discovery/owntracks.
#[tokio::test(flavor = "multi_thread")]
async fn seen_devices_published_as_discovery() {
    let (mosquitto, mut sup, observer) = setup().await;
    let mut mqtt = Mqtt::connect(mosquitto.port, "inventory-test").await;
    let sub = observer
        .declare_subscriber("home/discovery/owntracks")
        .await
        .expect("discovery subscriber");

    mqtt.publish(
        "owntracks/alice/phone",
        r#"{"_type":"location","lat":1.0,"lon":2.0}"#,
    )
    .await;
    let sample = tokio::time::timeout(Duration::from_secs(10), sub.recv_async())
        .await
        .expect("discovery document within 10s")
        .expect("discovery sample");
    let mut doc: Value =
        serde_json::from_slice(&sample.payload().to_bytes()).expect("discovery is JSON");
    assert_eq!(doc.as_array().expect("discovery is an array").len(), 1, "{doc}");

    // An unconfigured device shows up on its own traffic — the record set
    // grows and is republished, without disturbing the first record.
    mqtt.publish(
        "owntracks/bob/phone",
        r#"{"_type":"location","lat":3.0,"lon":4.0}"#,
    )
    .await;
    loop {
        let sample = tokio::time::timeout(Duration::from_secs(10), sub.recv_async())
            .await
            .expect("second discovery document within 10s")
            .expect("discovery sample");
        doc = serde_json::from_slice(&sample.payload().to_bytes()).expect("discovery is JSON");
        if doc.as_array().expect("discovery is an array").len() == 2 {
            break;
        }
    }

    let records = doc.as_array().expect("discovery is an array");
    let alice = records
        .iter()
        .find(|r| r["id"] == json!("alice/phone"))
        .expect("alice record");
    assert_eq!(alice["configured"], json!(true));
    assert_eq!(alice["entity"], json!("alice_phone"));
    assert_eq!(alice["suggested"], json!({"capability": "person", "features": []}));

    let bob = records
        .iter()
        .find(|r| r["id"] == json!("bob/phone"))
        .expect("bob record");
    assert_eq!(bob["configured"], json!(false));
    assert_eq!(bob["entity"], json!(null));
    assert_eq!(bob["suggested"], json!({"capability": "person", "features": []}));

    // The mirror serves it to late joiners.
    let replies = observer.get("home/discovery/owntracks").await.expect("get discovery");
    let reply = replies.recv_async().await.expect("mirrored discovery reply");
    let mirrored: Value = serde_json::from_slice(
        &reply.result().expect("mirrored sample").payload().to_bytes(),
    )
    .expect("mirrored discovery is JSON");
    assert_eq!(mirrored, doc, "mirror serves the same document");

    sup.shutdown();
}
