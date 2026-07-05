//! Zigbee2MQTT adapter integration tests: each scenario spawns a real
//! mosquitto broker on a free port plus the real supervisor on the z2m
//! fixture house, and asserts on both buses.

mod common;

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use homeostat::bus::HealthStatus;
use rumqttc::{AsyncClient, Event, Incoming, MqttOptions, QoS};
use serde_json::{json, Value};
use zenoh::handlers::FifoChannelHandler;
use zenoh::pubsub::Subscriber;
use zenoh::sample::{Sample, SampleKind};

use common::{await_health, free_port, health_watch, process_alive, Supervisor};

const FIXTURE: &str = "tests/fixture_house_z2m";
const PORT_ENV: &str = "HOMEOSTAT_TEST_MQTT_PORT";
const EVENT_KEY: &str = "home/health/zigbee/event";

/// A mosquitto broker on a free port, killed on drop.
struct Mosquitto {
    child: Child,
    port: u16,
    conf: PathBuf,
}

impl Mosquitto {
    fn spawn() -> Self {
        let port = free_port();
        let conf = std::env::temp_dir().join(format!("homeostat-z2m-{port}.conf"));
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

/// An MQTT test client: publishes are acknowledged (QoS 1) before returning,
/// incoming messages are buffered so acks and messages can interleave.
struct Mqtt {
    client: AsyncClient,
    events: tokio::sync::mpsc::UnboundedReceiver<Event>,
    inbox: VecDeque<(String, Vec<u8>)>,
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
        let mut mqtt = Self { client, events, inbox: VecDeque::new() };
        mqtt.await_event(|i| matches!(i, Incoming::ConnAck(_))).await;
        mqtt
    }

    async fn subscribe(&mut self, topic: &str) {
        self.client
            .subscribe(topic, QoS::AtLeastOnce)
            .await
            .expect("mqtt subscribe");
        self.await_event(|i| matches!(i, Incoming::SubAck(_))).await;
    }

    async fn publish(&mut self, topic: &str, payload: &str) {
        self.client
            .publish(topic, QoS::AtLeastOnce, false, payload)
            .await
            .expect("mqtt publish");
        self.await_event(|i| matches!(i, Incoming::PubAck(_))).await;
    }

    /// Reads events until `pred` matches, buffering message publishes.
    async fn await_event<F: Fn(&Incoming) -> bool>(&mut self, pred: F) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let event = tokio::time::timeout_at(deadline, self.events.recv())
                .await
                .expect("mqtt event within 10s")
                .expect("mqtt event loop alive");
            match event {
                Event::Incoming(Incoming::Publish(p)) => {
                    self.inbox.push_back((p.topic.clone(), p.payload.to_vec()));
                }
                Event::Incoming(incoming) if pred(&incoming) => return,
                _ => {}
            }
        }
    }

    /// Next subscribed message, or None if the timeout elapses first.
    async fn next_message(&mut self, timeout: Duration) -> Option<(String, Vec<u8>)> {
        if let Some(msg) = self.inbox.pop_front() {
            return Some(msg);
        }
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let event = tokio::time::timeout_at(deadline, self.events.recv())
                .await
                .ok()?
                .expect("mqtt event loop alive");
            if let Event::Incoming(Incoming::Publish(p)) = event {
                return Some((p.topic.clone(), p.payload.to_vec()));
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
        .declare_subscriber("home/health/zigbee/alive")
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

type StateSub = Subscriber<FifoChannelHandler<Sample>>;

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

/// (a) Scripted z2m state publishes translate to per-aspect state keys.
#[tokio::test(flavor = "multi_thread")]
async fn z2m_state_translates_to_bus_state() {
    let (mosquitto, mut sup, observer) = setup().await;
    let state_sub = observer
        .declare_subscriber("home/state/**")
        .await
        .expect("state subscriber");
    let mut mqtt = Mqtt::connect(mosquitto.port, "test-state").await;

    mqtt.publish("zigbee2mqtt/lamp_kitchen_1", r#"{"state":"ON","brightness":128}"#)
        .await;
    expect_states(
        &state_sub,
        &[
            ("home/state/kitchen/kitchen_lamp/on", json!(true)),
            ("home/state/kitchen/kitchen_lamp/brightness", json!(128)),
        ],
    )
    .await;

    // Locks are state-only: state still translates (normalized to `locked`).
    mqtt.publish("zigbee2mqtt/lock_front_1", r#"{"state":"LOCKED"}"#).await;
    expect_states(&state_sub, &[("home/state/hallway/front_door/locked", json!(true))]).await;

    sup.shutdown();
}

/// (b) Bus commands translate to zigbee2mqtt/{device}/set; lock commands
/// stay unwired until the arbiter exists.
#[tokio::test(flavor = "multi_thread")]
async fn bus_commands_translate_to_z2m_set() {
    let (mosquitto, mut sup, observer) = setup().await;
    let mut mqtt = Mqtt::connect(mosquitto.port, "test-cmd").await;
    mqtt.subscribe("zigbee2mqtt/+/set").await;

    observer
        .put("home/cmd/kitchen/kitchen_lamp/on", "true")
        .await
        .expect("cmd put");
    let (topic, payload) = mqtt
        .next_message(Duration::from_secs(10))
        .await
        .expect("set publish for on command");
    assert_eq!(topic, "zigbee2mqtt/lamp_kitchen_1/set");
    let payload: Value = serde_json::from_slice(&payload).expect("set payload is JSON");
    assert_eq!(payload, json!({"state": "ON"}));

    observer
        .put("home/cmd/kitchen/kitchen_lamp/brightness", "200")
        .await
        .expect("cmd put");
    let (topic, payload) = mqtt
        .next_message(Duration::from_secs(10))
        .await
        .expect("set publish for brightness command");
    assert_eq!(topic, "zigbee2mqtt/lamp_kitchen_1/set");
    let payload: Value = serde_json::from_slice(&payload).expect("set payload is JSON");
    assert_eq!(payload, json!({"brightness": 200}));

    // The adapter does not even subscribe to lock command keys.
    observer
        .put("home/cmd/hallway/front_door/locked", "true")
        .await
        .expect("cmd put");
    let silence = mqtt.next_message(Duration::from_millis(1500)).await;
    assert!(silence.is_none(), "lock command reached MQTT: {silence:?}");

    sup.shutdown();
}

/// (c) Unknown devices and malformed payloads are dropped without crashing,
/// each with a health event, and translation keeps working afterwards.
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

    mqtt.publish("zigbee2mqtt/ghost_device", r#"{"state":"ON"}"#).await;
    expect_drop_event(&event_sub, "unknown-device").await;

    mqtt.publish("zigbee2mqtt/lamp_kitchen_1", "certainly not json").await;
    expect_drop_event(&event_sub, "malformed-payload").await;

    // Still alive and translating.
    mqtt.publish("zigbee2mqtt/lamp_kitchen_1", r#"{"state":"OFF"}"#).await;
    expect_states(&state_sub, &[("home/state/kitchen/kitchen_lamp/on", json!(false))]).await;

    sup.shutdown();
}

/// (d) The adapter honors the step-2 unit contract: liveliness token when
/// ready, clean SIGTERM shutdown within the grace, no orphans.
#[tokio::test(flavor = "multi_thread")]
async fn adapter_honors_unit_contract() {
    let (_mosquitto, mut sup, observer) = setup().await;
    let mut watch = health_watch(&observer, "zigbee").await;
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

/// (e) Discovery: a bridge/devices inventory lands on the bus as one JSON
/// document at home/discovery/zigbee — binding ids, configured flags,
/// best-effort suggestions, raw definitions; the coordinator is omitted.
#[tokio::test(flavor = "multi_thread")]
async fn bridge_inventory_published_as_discovery() {
    let (mosquitto, mut sup, observer) = setup().await;
    let mut mqtt = Mqtt::connect(mosquitto.port, "inventory-test").await;
    let sub = observer
        .declare_subscriber("home/discovery/zigbee")
        .await
        .expect("discovery subscriber");

    mqtt.publish(
        "zigbee2mqtt/bridge/devices",
        &json!([
            {"type": "Coordinator", "friendly_name": "Coordinator", "ieee_address": "0x00"},
            {"type": "Router", "friendly_name": "lamp_kitchen_1", "ieee_address": "0x01",
             "definition": {"vendor": "IKEA", "model": "LED1836G9", "description": "bulb",
                "exposes": [{"type": "light",
                    "features": [{"property": "state"}, {"property": "brightness"}]}]}},
            {"type": "EndDevice", "friendly_name": "motion_new", "ieee_address": "0x02",
             "definition": {"vendor": "Aqara", "model": "RTCGQ11LM", "description": "motion",
                "exposes": [{"type": "binary", "property": "occupancy"}]}}
        ])
        .to_string(),
    )
    .await;

    let sample = tokio::time::timeout(Duration::from_secs(10), sub.recv_async())
        .await
        .expect("discovery document within 10s")
        .expect("discovery sample");
    let doc: Value =
        serde_json::from_slice(&sample.payload().to_bytes()).expect("discovery is JSON");
    let records = doc.as_array().expect("discovery is an array");
    assert_eq!(records.len(), 2, "coordinator omitted: {doc}");

    let lamp = records
        .iter()
        .find(|r| r["id"] == json!("lamp_kitchen_1"))
        .expect("lamp record");
    assert_eq!(lamp["configured"], json!(true));
    assert_eq!(lamp["entity"], json!("kitchen_lamp"));
    assert_eq!(lamp["suggested"]["capability"], json!("light"));
    assert_eq!(lamp["suggested"]["features"], json!(["brightness"]));

    let motion = records
        .iter()
        .find(|r| r["id"] == json!("motion_new"))
        .expect("motion record");
    assert_eq!(motion["configured"], json!(false));
    assert_eq!(motion["entity"], json!(null));
    assert_eq!(motion["suggested"]["capability"], json!("presence"));
    assert_eq!(motion["description"]["model"], json!("RTCGQ11LM"));

    // The mirror serves it to late joiners: the read path the MCP
    // surface's read_state uses.
    let replies = observer.get("home/discovery/zigbee").await.expect("get discovery");
    let reply = replies.recv_async().await.expect("mirrored discovery reply");
    let mirrored: Value = serde_json::from_slice(
        &reply.result().expect("mirrored sample").payload().to_bytes(),
    )
    .expect("mirrored discovery is JSON");
    assert_eq!(mirrored, doc, "mirror serves the same document");

    sup.shutdown();
}
