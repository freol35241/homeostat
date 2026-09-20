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
use zenoh::sample::SampleKind;

use common::{expect_event_kind, expect_state, free_port, Supervisor};

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
            assert!(
                Instant::now() < deadline,
                "fake openwrt router never listened on {port}"
            );
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
        stream
            .read_to_string(&mut response)
            .expect("read control response");
        assert!(
            response.starts_with("HTTP/1.1 200"),
            "control {path}: {response}"
        );
    }
}

impl Drop for FakeOpenwrt {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Writes a HOMEOSTAT_OPENWRT file (outside the repo, per the settlement)
/// giving each named router a fake endpoint's host and the read-only rpcd
/// credentials. The fixture binds entities on "gw"; any further router is
/// configured but unbound, which is all a second AP needs to be polled.
fn routers_file(routers: &[(&str, u16)]) -> PathBuf {
    let tag: Vec<String> = routers.iter().map(|(_, port)| port.to_string()).collect();
    let path = std::env::temp_dir().join(format!("homeostat-openwrt-{}.toml", tag.join("-")));
    let body: String = routers
        .iter()
        .map(|(name, port)| {
            format!(
                "[{name}]\nhost = \"127.0.0.1:{port}\"\nusername = \"{USERNAME}\"\npassword = \"{PASSWORD}\"\n"
            )
        })
        .collect();
    std::fs::write(&path, body).expect("write routers file");
    path
}

/// Spawns the fake router + supervisor on the fixture and waits for the
/// adapter's liveliness token (generous timeout: first run resolves the
/// uv env for aiohttp too).
async fn setup() -> (FakeOpenwrt, PathBuf, Supervisor, zenoh::Session) {
    let router = FakeOpenwrt::spawn();
    let routers_path = routers_file(&[("gw", router.port)]);
    let (sup, observer) = start(&routers_path).await;
    (router, routers_path, sup, observer)
}

/// The supervisor on the fixture against a written routers file, up to the
/// adapter's liveliness token.
async fn start(routers_path: &std::path::Path) -> (Supervisor, zenoh::Session) {
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
    (sup, observer)
}

/// (a) A phone associating to an AP becomes `presence = true` on the bus;
/// dropping off flips it back after the away delay — and the discovery
/// feed shows the sighted station bound to its entity.
#[tokio::test(flavor = "multi_thread")]
async fn wifi_association_drives_presence() {
    let (router, _routers_path, mut sup, observer) = setup().await;
    let presence_sub = observer
        .declare_subscriber(PRESENCE_KEY)
        .await
        .expect("presence subscriber");
    let discovery_sub = observer
        .declare_subscriber(DISCOVERY_KEY)
        .await
        .expect("discovery subscriber");

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
            assert_eq!(
                record["entity"],
                json!("dads_phone"),
                "phone record: {record}"
            );
            // and its aspect descriptor: the one boolean this capability speaks
            assert_eq!(
                record["aspects"]["fields"]["presence"]["kind"],
                json!("boolean"),
                "{record}"
            );
            assert_eq!(
                record["aspects"]["fields"]["presence"]["values"][1]["label"],
                json!("away")
            );
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
    let wan_sub = observer
        .declare_subscriber(WAN_KEY)
        .await
        .expect("wan subscriber");
    let site_sub = observer
        .declare_subscriber(SITE_TUNNEL_KEY)
        .await
        .expect("site tunnel subscriber");
    let office_sub = observer
        .declare_subscriber(OFFICE_TUNNEL_KEY)
        .await
        .expect("office tunnel subscriber");

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
    let wan_sub = observer
        .declare_subscriber(WAN_KEY)
        .await
        .expect("wan subscriber");
    let event_sub = observer
        .declare_subscriber(EVENT_KEY)
        .await
        .expect("event subscriber");

    router.control("/control/break");
    expect_event_kind(&event_sub, "router-unreachable").await;

    router.control("/control/restore");
    router.control("/control/wan?up=false");
    expect_state(&wan_sub, json!(false)).await;

    sup.shutdown();
}

/// (e) A router whose replies stream is read whole. aiohttp's
/// `content.read(n)` hands back only what is buffered — the first chunk of
/// a chunked reply — so a single read truncates the JSON and every decode
/// fails. rpcd itself answers with Content-Length, which is why this has
/// never bitten in production here; the identical read in the onvif
/// adapter faced firmware that does stream, and took both of VP52's
/// cameras down on v0.12.0. The fix belongs to both call sites, so the
/// test does too.
///
/// Chunking is switched on mid-run, after presence has already been proved
/// to work, so the assertion is about the framing and nothing else.
#[tokio::test(flavor = "multi_thread")]
async fn a_chunked_ubus_reply_is_read_whole() {
    let (router, _routers_path, mut sup, observer) = setup().await;
    let presence_sub = observer
        .declare_subscriber(PRESENCE_KEY)
        .await
        .expect("presence subscriber");

    // The control: unchunked, presence follows the station.
    router.control(&format!("/control/station?mac={PHONE_MAC}&present=true"));
    expect_state(&presence_sub, json!(true)).await;

    router.control("/control/chunked");
    router.control(&format!("/control/station?mac={PHONE_MAC}&present=false"));
    expect_state(&presence_sub, json!(false)).await;

    sup.shutdown();
}

/// (f) Partial blindness is not absence. Two routers configured, the phone
/// associated only to the AP: when the AP goes silent the phone must hold
/// stale, because the union of the routers that *did* answer says nothing
/// about a device that lives on the one that did not — VP52 published
/// `presence = false` for 37 hours with the family at home (#144). The
/// blind spot is announced, and absence becomes assertable again once
/// every router answers.
#[tokio::test(flavor = "multi_thread")]
async fn a_silent_router_cannot_assert_absence() {
    let gw = FakeOpenwrt::spawn();
    let ap = FakeOpenwrt::spawn();
    let routers_path = routers_file(&[("gw", gw.port), ("ap", ap.port)]);
    let (mut sup, observer) = start(&routers_path).await;
    let presence_sub = observer
        .declare_subscriber(PRESENCE_KEY)
        .await
        .expect("presence subscriber");
    let event_sub = observer
        .declare_subscriber(EVENT_KEY)
        .await
        .expect("event subscriber");

    ap.control(&format!("/control/station?mac={PHONE_MAC}&present=true"));
    expect_state(&presence_sub, json!(true)).await;

    ap.control("/control/break");
    expect_event_kind(&event_sub, "presence-partial").await;

    // Several poll cycles past the 2s away delay: nothing may say away.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    while let Ok(sample) = tokio::time::timeout_at(deadline, presence_sub.recv_async()).await {
        let value: Value = serde_json::from_slice(&sample.expect("state stream open").payload().to_bytes())
            .expect("state payload is JSON");
        assert_ne!(
            value,
            json!(false),
            "presence asserted away while the AP that sees the phone was silent"
        );
    }

    // The AP answering again, the phone really gone: away, as before.
    ap.control("/control/restore");
    ap.control(&format!("/control/station?mac={PHONE_MAC}&present=false"));
    expect_state(&presence_sub, json!(false)).await;

    sup.shutdown();
}
