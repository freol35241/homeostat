//! OpenWrt adapter integration tests: each scenario spawns a fake ubus
//! endpoint (tests/fake_openwrt.py — real JSON-RPC shapes, real session-id
//! enforcement) on a free port plus the real supervisor on the openwrt
//! fixture house, and asserts on the bus. The fixture polls every second
//! with a 2s presence away-delay, so transitions land within the timeouts.

mod common;

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use zenoh::handlers::FifoChannelHandler;
use zenoh::pubsub::Subscriber;
use zenoh::sample::{Sample, SampleKind};

use common::{free_port, Supervisor};

const FIXTURE: &str = "tests/fixture_house_openwrt";
const ROUTERS_ENV: &str = "HOMEOSTAT_OPENWRT";
const EVENT_KEY: &str = "home/health/openwrt/event";
const WAN_KEY: &str = "home/state/hallway/gateway/wan";
const SITE_TUNNEL_KEY: &str = "home/state/global/site_tunnel/up";
const OFFICE_TUNNEL_KEY: &str = "home/state/global/office_tunnel/up";
const PRESENCE_KEY: &str = "home/state/global/dads_phone/presence";
const DISCOVERY_KEY: &str = "home/discovery/openwrt";
const PHONE_MAC: &str = "aa:bb:cc:dd:ee:ff";
const USERNAME: &str = "homeostat";
const PASSWORD: &str = "secret123";

/// A fake ubus endpoint (tests/fake_openwrt.py) on a free port, killed on
/// drop. Spawned the same way the units themselves are: `uv run`.
struct FakeOpenwrt {
    child: Child,
    port: u16,
}

impl FakeOpenwrt {
    fn spawn() -> Self {
        let port = free_port();
        let child = Command::new("uv")
            .args([
                "run",
                "tests/fake_openwrt.py",
                "--port",
                &port.to_string(),
                "--username",
                USERNAME,
                "--password",
                PASSWORD,
            ])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn fake openwrt router (is uv installed?)");
        // First run resolves the fake router's own uv env: generous.
        let deadline = Instant::now() + Duration::from_secs(60);
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(Instant::now() < deadline, "fake openwrt router never listened on {port}");
            std::thread::sleep(Duration::from_millis(50));
        }
        Self { child, port }
    }

    /// POSTs a control endpoint (plain HTTP, out of the JSON-RPC path).
    fn control(&self, path: &str) {
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", self.port))
            .expect("connect to fake router control");
        stream
            .write_all(
                format!(
                    "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .expect("write control request");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("read control response");
        assert!(response.starts_with("HTTP/1.1 200"), "control {path}: {response}");
    }
}

impl Drop for FakeOpenwrt {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Writes a HOMEOSTAT_OPENWRT file (outside the repo, per the settlement)
/// giving the fixture's "gw" router the fake endpoint's host and the
/// read-only rpcd credentials.
fn routers_file(port: u16) -> PathBuf {
    let path = std::env::temp_dir().join(format!("homeostat-openwrt-{port}.toml"));
    std::fs::write(
        &path,
        format!(
            "[gw]\nhost = \"127.0.0.1:{port}\"\nusername = \"{USERNAME}\"\npassword = \"{PASSWORD}\"\n"
        ),
    )
    .expect("write routers file");
    path
}

/// Spawns the fake router + supervisor on the fixture and waits for the
/// adapter's liveliness token (generous timeout: first run resolves the
/// uv env for aiohttp too).
async fn setup() -> (FakeOpenwrt, PathBuf, Supervisor, zenoh::Session) {
    let router = FakeOpenwrt::spawn();
    let routers_path = routers_file(router.port);
    let sup = Supervisor::spawn_with_env(
        FIXTURE,
        &[(ROUTERS_ENV, routers_path.to_str().expect("utf-8 path"))],
    );
    let observer = sup.observer().await;
    let token_sub = observer
        .liveliness()
        .declare_subscriber("home/health/openwrt/alive")
        .history(true)
        .await
        .expect("liveliness subscriber");
    let token = tokio::time::timeout(Duration::from_secs(90), token_sub.recv_async())
        .await
        .expect("adapter liveliness token within 90s")
        .expect("liveliness stream open");
    assert_eq!(token.kind(), SampleKind::Put);
    (router, routers_path, sup, observer)
}

type StateSub = Subscriber<FifoChannelHandler<Sample>>;

/// Waits for a key to carry `expected`.
async fn expect_state(sub: &StateSub, expected: Value) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let sample = tokio::time::timeout_at(deadline, sub.recv_async())
            .await
            .unwrap_or_else(|_| panic!("no value {expected} within 20s"))
            .expect("state stream open");
        let value: Value = serde_json::from_slice(&sample.payload().to_bytes())
            .expect("state payload is JSON");
        if value == expected {
            return;
        }
    }
}

/// Reads health events until one matches the expected drop reason.
async fn expect_drop_event(sub: &StateSub, reason: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let sample = tokio::time::timeout_at(deadline, sub.recv_async())
            .await
            .unwrap_or_else(|_| panic!("no \"{reason}\" health event within 20s"))
            .expect("event stream open");
        let event: Value = serde_json::from_slice(&sample.payload().to_bytes())
            .expect("health event is JSON");
        assert_eq!(event["kind"], "drop", "unexpected event kind: {event}");
        if event["reason"] == reason {
            return;
        }
    }
}

/// (a) A phone associating to an AP becomes `presence = true` on the bus;
/// dropping off flips it back after the away delay — and the discovery
/// feed shows the sighted station bound to its entity.
#[tokio::test(flavor = "multi_thread")]
async fn wifi_association_drives_presence() {
    let (router, _routers_path, mut sup, observer) = setup().await;
    let presence_sub =
        observer.declare_subscriber(PRESENCE_KEY).await.expect("presence subscriber");
    let discovery_sub =
        observer.declare_subscriber(DISCOVERY_KEY).await.expect("discovery subscriber");

    router.control(&format!("/control/station?mac={PHONE_MAC}&present=true"));
    expect_state(&presence_sub, json!(true)).await;

    // The station's appearance changes the periphery: one JSON array,
    // the phone's record bound to its entity.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let sample = tokio::time::timeout_at(deadline, discovery_sub.recv_async())
            .await
            .expect("discovery publish within 20s")
            .expect("discovery stream open");
        let records: Value = serde_json::from_slice(&sample.payload().to_bytes())
            .expect("discovery payload is JSON");
        let phone = records
            .as_array()
            .expect("discovery payload is an array")
            .iter()
            .find(|r| r["id"] == PHONE_MAC);
        if let Some(record) = phone {
            assert_eq!(record["configured"], json!(true), "phone record: {record}");
            assert_eq!(record["entity"], json!("dads_phone"), "phone record: {record}");
            break;
        }
    }

    router.control(&format!("/control/station?mac={PHONE_MAC}&present=false"));
    expect_state(&presence_sub, json!(false)).await;

    sup.shutdown();
}

/// (b) Connectivity state: WAN down is a `wan = false` transition; a
/// stale WireGuard handshake takes the tunnel down even though the
/// interface stays up; a service-managed tunnel follows its interface's
/// own up flag; a fresh handshake brings the WireGuard tunnel back.
#[tokio::test(flavor = "multi_thread")]
async fn connectivity_state_translates_to_bus() {
    let (router, _routers_path, mut sup, observer) = setup().await;
    let wan_sub = observer.declare_subscriber(WAN_KEY).await.expect("wan subscriber");
    let site_sub =
        observer.declare_subscriber(SITE_TUNNEL_KEY).await.expect("site tunnel subscriber");
    let office_sub =
        observer.declare_subscriber(OFFICE_TUNNEL_KEY).await.expect("office tunnel subscriber");

    router.control("/control/wan?up=false");
    expect_state(&wan_sub, json!(false)).await;

    router.control("/control/wg?age=9999");
    expect_state(&site_sub, json!(false)).await;

    router.control("/control/tunnel?iface=vpn0&up=false");
    expect_state(&office_sub, json!(false)).await;

    router.control("/control/wg?age=5");
    expect_state(&site_sub, json!(true)).await;

    sup.shutdown();
}

/// (c) An unreachable router emits one "router-unreachable" health event
/// and its aspects go stale rather than false; polling resumes after
/// recovery without a restart.
#[tokio::test(flavor = "multi_thread")]
async fn unreachable_router_drops_once_and_recovers() {
    let (router, _routers_path, mut sup, observer) = setup().await;
    let wan_sub = observer.declare_subscriber(WAN_KEY).await.expect("wan subscriber");
    let event_sub = observer.declare_subscriber(EVENT_KEY).await.expect("event subscriber");

    router.control("/control/break");
    expect_drop_event(&event_sub, "router-unreachable").await;

    router.control("/control/restore");
    router.control("/control/wan?up=false");
    expect_state(&wan_sub, json!(false)).await;

    sup.shutdown();
}
