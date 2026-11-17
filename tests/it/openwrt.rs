//! OpenWrt adapter integration tests: each scenario spawns a fake ubus
//! endpoint (`tests/fake_openwrt.py`, with real JSON-RPC shapes and
//! session-id enforcement) on a free port plus the real supervisor on the
//! openwrt fixture house, and asserts on the bus. The fixture polls every
//! second with a 2s away-delay, so sighting transitions land within the
//! timeouts.

use crate::common;

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::common::{
    await_sample, config_write, expect_event_kind, expect_state, fixture_command, free_port,
    next_sample, restart_unit, up, Scratch, Supervisor,
};

const FIXTURE: &str = "tests/fixtures/house_openwrt";
const ROUTERS_ENV: &str = "HOMEOSTAT_OPENWRT";
const EVENT_KEY: &str = "home/health/openwrt/event";
const WAN_KEY: &str = "home/state/hallway/gateway/wan";
const SIGHTED_KEY: &str = "home/state/person/dads_phone/sighted";
const AVAILABLE_KEY: &str = "home/state/person/dads_phone/available";
const DISCOVERY_KEY: &str = "home/discovery/openwrt";
const PHONE_MAC: &str = "aa:bb:cc:dd:ee:ff";
const USERNAME: &str = "homeostat";
const PASSWORD: &str = "secret123";

/// A fake ubus endpoint (`tests/fake_openwrt.py`) on a free port, killed on
/// drop. Spawned the same way the units themselves are: out of the
/// script's own uv environment, exec'd directly (`fixture_command`),
/// so the handle held here is the interpreter's and not uv's.
struct FakeOpenwrt {
    child: Child,
    port: u16,
}

impl FakeOpenwrt {
    async fn spawn() -> Self {
        let port = free_port();
        let (program, args) = fixture_command(&format!(
            "uv run tests/fake_openwrt.py --port {port} --username {USERNAME} --password {PASSWORD}"
        ))
        .await;
        let child = Command::new(program)
            .args(args)
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn fake openwrt router (is uv installed?)");
        // First run resolves the fake router's own uv env: generous.
        let deadline = Instant::now() + Duration::from_mins(1);
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

/// Writes a `HOMEOSTAT_OPENWRT` file in `scratch` (outside the repo: it
/// holds credentials) giving each named router a fake endpoint's host and
/// the read-only rpcd credentials. The fixture binds entities on "gw"; any
/// further router is configured but unbound, which is all a second AP needs
/// to be polled.
fn routers_file(scratch: &Scratch, routers: &[(&str, u16)]) -> PathBuf {
    let plain: Vec<(&str, u16, &str)> = routers.iter().map(|&(n, p)| (n, p, "")).collect();
    routers_file_with(scratch, &plain)
}

/// As `routers_file`, with extra TOML lines for each router's entry.
fn routers_file_with(scratch: &Scratch, routers: &[(&str, u16, &str)]) -> PathBuf {
    let body = routers
        .iter()
        .map(|(name, port, extra)| {
            format!(
                "[{name}]\nhost = \"127.0.0.1:{port}\"\nusername = \"{USERNAME}\"\npassword = \"{PASSWORD}\"\n{extra}"
            )
        })
        .collect::<Vec<_>>()
        .concat();
    scratch.file("routers.toml", &body)
}

/// Spawns the fake router + supervisor on the fixture and waits for the
/// adapter to run.
async fn setup() -> (FakeOpenwrt, Scratch, Supervisor, zenoh::Session) {
    let router = FakeOpenwrt::spawn().await;
    let routers = Scratch::new("openwrt-routers");
    let routers_path = routers_file(&routers, &[("gw", router.port)]);
    let (sup, observer) = start(&routers_path).await;
    (router, routers, sup, observer)
}

/// The supervisor on the fixture against a written routers file, up to the
/// adapter running.
async fn start(routers_path: &std::path::Path) -> (Supervisor, zenoh::Session) {
    up(
        || {
            Supervisor::spawn_with_env(
                FIXTURE,
                &[(ROUTERS_ENV, routers_path.to_str().expect("utf-8 path"))],
            )
        },
        &["openwrt"],
    )
    .await
}

/// (a) A phone associating to an AP becomes `sighted = true` on the bus;
/// dropping off flips it back after the away delay. The discovery feed
/// shows the sighted station bound to its entity.
#[tokio::test(flavor = "multi_thread")]
async fn wifi_association_drives_sighted() {
    let (router, _routers, mut sup, observer) = setup().await;
    let sighted_sub = observer
        .declare_subscriber(SIGHTED_KEY)
        .await
        .expect("sighted subscriber");
    let discovery_sub = observer
        .declare_subscriber(DISCOVERY_KEY)
        .await
        .expect("discovery subscriber");

    router.control(&format!("/control/station?mac={PHONE_MAC}&present=true"));
    expect_state(&sighted_sub, json!(true)).await;

    // The station's appearance changes the periphery: one JSON array,
    // the phone's record bound to its entity.
    let phone = |records: &Value| {
        records
            .as_array()
            .expect("discovery payload is an array")
            .iter()
            .find(|r| r["id"] == PHONE_MAC)
            .cloned()
    };
    let doc = await_sample(
        &discovery_sub,
        Duration::from_secs(20),
        "discovery publish naming the phone",
        |msg| phone(msg.value()).is_some(),
    )
    .await;
    let record = phone(doc.value()).expect("the phone's record");
    assert_eq!(record["configured"], json!(true), "phone record: {record}");
    assert_eq!(
        record["entity"],
        json!("dads_phone"),
        "phone record: {record}"
    );
    // and its aspect descriptor: the one boolean this capability speaks
    assert_eq!(
        record["aspects"]["fields"]["sighted"]["kind"],
        json!("boolean"),
        "{record}"
    );
    assert_eq!(
        record["aspects"]["fields"]["sighted"]["values"][1]["label"],
        json!("not seen")
    );

    router.control(&format!("/control/station?mac={PHONE_MAC}&present=false"));
    expect_state(&sighted_sub, json!(false)).await;

    sup.shutdown();
}

/// (b) Connectivity state: WAN down is a `wan = false` transition, and
/// back up again. Tunnels are not this adapter's
/// (docs/design.md#the-capability-vocabulary): it reports sightings and WAN.
#[tokio::test(flavor = "multi_thread")]
async fn connectivity_state_translates_to_bus() {
    let (router, _routers, mut sup, observer) = setup().await;
    let wan_sub = observer
        .declare_subscriber(WAN_KEY)
        .await
        .expect("wan subscriber");

    router.control("/control/wan?up=false");
    expect_state(&wan_sub, json!(false)).await;

    router.control("/control/wan?up=true");
    expect_state(&wan_sub, json!(true)).await;

    sup.shutdown();
}

/// (c) An unreachable router emits one "router-unreachable" health event
/// and its aspects go stale rather than false; polling resumes after
/// recovery without a restart.
#[tokio::test(flavor = "multi_thread")]
async fn unreachable_router_drops_once_and_recovers() {
    let (router, _routers, mut sup, observer) = setup().await;
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
/// `content.read(n)` hands back only what is buffered (the first chunk of
/// a chunked reply), so a single read truncates the JSON and every decode
/// fails. rpcd itself answers with Content-Length, so stock rpcd does not
/// show this; the onvif adapter makes the same read against
/// firmware that does stream. The whole-body read belongs to both call
/// sites, so the test does too.
///
/// Chunking is switched on mid-run, after sightings have already been proved
/// to work, so the assertion is about the framing and nothing else.
#[tokio::test(flavor = "multi_thread")]
async fn a_chunked_ubus_reply_is_read_whole() {
    let (router, _routers, mut sup, observer) = setup().await;
    let sighted_sub = observer
        .declare_subscriber(SIGHTED_KEY)
        .await
        .expect("sighted subscriber");

    // The control: unchunked, the sighting follows the station.
    router.control(&format!("/control/station?mac={PHONE_MAC}&present=true"));
    expect_state(&sighted_sub, json!(true)).await;

    router.control("/control/chunked");
    router.control(&format!("/control/station?mac={PHONE_MAC}&present=false"));
    expect_state(&sighted_sub, json!(false)).await;

    sup.shutdown();
}

/// (f) A silent router does not make its stations unseen. Two routers are
/// configured and the phone is associated only to the AP. When the AP
/// goes silent the phone must hold stale, because the routers that did
/// answer say nothing about a device on the one that did not. Reading it
/// as unseen would publish `sighted = false` with the family at home for
/// as long as the AP stays silent. The blind spot is announced, the
/// tracker turns unavailable so its held value is not read as current,
/// and both recover once every router answers.
#[tokio::test(flavor = "multi_thread")]
async fn a_silent_router_cannot_assert_absence() {
    let gw = FakeOpenwrt::spawn().await;
    let ap = FakeOpenwrt::spawn().await;
    let routers = Scratch::new("openwrt-routers");
    let routers_path = routers_file(&routers, &[("gw", gw.port), ("ap", ap.port)]);
    let (mut sup, observer) = start(&routers_path).await;
    let sighted_sub = observer
        .declare_subscriber(SIGHTED_KEY)
        .await
        .expect("sighted subscriber");
    let available_sub = observer
        .declare_subscriber(AVAILABLE_KEY)
        .await
        .expect("available subscriber");
    let event_sub = observer
        .declare_subscriber(EVENT_KEY)
        .await
        .expect("event subscriber");

    ap.control(&format!("/control/station?mac={PHONE_MAC}&present=true"));
    expect_state(&sighted_sub, json!(true)).await;

    ap.control("/control/break");
    expect_event_kind(&event_sub, "presence-partial").await;
    expect_state(&available_sub, json!(false)).await;

    // Several poll cycles past the 2s away delay: nothing may say away.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    while let Some(sample) = next_sample(
        &sighted_sub,
        deadline.saturating_duration_since(tokio::time::Instant::now()),
    )
    .await
    {
        assert_ne!(
            *sample.value(),
            json!(false),
            "sighted asserted false while the AP that sees the phone was silent"
        );
    }

    // The AP answering again, the phone really gone: not seen, as before.
    ap.control("/control/restore");
    ap.control(&format!("/control/station?mac={PHONE_MAC}&present=false"));
    expect_state(&sighted_sub, json!(false)).await;
    expect_state(&available_sub, json!(true)).await;

    sup.shutdown();
}

/// The phone's state samples, in arrival order, until its `fixed_at`.
async fn until_fix(sub: &common::StateSub) -> Vec<(String, Value)> {
    let mut seen = Vec::new();
    await_sample(sub, Duration::from_secs(20), "fixed_at", |msg| {
        let aspect = msg.key.rsplit('/').next().unwrap().to_string();
        let done = aspect == "fixed_at";
        seen.push((aspect, msg.value().clone()));
        done
    })
    .await;
    seen
}

/// Fails if the phone publishes a `fixed_at` within `secs`.
async fn no_fix_for(sub: &common::StateSub, secs: u64) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while let Some(sample) = next_sample(
        sub,
        deadline.saturating_duration_since(tokio::time::Instant::now()),
    )
    .await
    {
        assert!(
            !sample.key.ends_with("/fixed_at"),
            "unexpected fix: {sample:?}"
        );
    }
}

/// (g) A router with a location turns a sighting into a fix at the router,
/// with its radius as the accuracy and `fixed_at` last. The fix comes when
/// the device comes into range and not on every poll, so a held sighting
/// cannot outdate the phone's own fixes. A router without a location
/// gives none.
#[tokio::test(flavor = "multi_thread")]
async fn a_sighting_at_a_located_router_is_a_fix() {
    let gw = FakeOpenwrt::spawn().await;
    let ap = FakeOpenwrt::spawn().await;
    let routers = Scratch::new("openwrt-routers");
    let routers_path = routers_file_with(
        &routers,
        &[
            ("gw", gw.port, "lat = 59.33\nlon = 18.06\nradius_m = 40\n"),
            ("ap", ap.port, ""),
        ],
    );
    let (mut sup, observer) = start(&routers_path).await;
    let phone_sub = observer
        .declare_subscriber("home/state/person/dads_phone/*")
        .await
        .expect("phone subscriber");
    let sighted_sub = observer
        .declare_subscriber(SIGHTED_KEY)
        .await
        .expect("sighted subscriber");

    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    gw.control(&format!("/control/station?mac={PHONE_MAC}&present=true"));
    let seen = until_fix(&phone_sub).await;
    let at = |aspect: &str| seen.iter().position(|(k, _)| k == aspect);
    for (aspect, value) in [
        ("lat", json!(59.33)),
        ("lon", json!(18.06)),
        ("accuracy", json!(40.0)),
    ] {
        let i = at(aspect).unwrap_or_else(|| panic!("no {aspect} before fixed_at: {seen:?}"));
        assert_eq!(seen[i].1, value, "{seen:?}");
    }
    let fixed_at = seen
        .last()
        .unwrap()
        .1
        .as_f64()
        .expect("fixed_at is a number");
    assert!(
        fixed_at >= before - 1.0,
        "fixed_at {fixed_at} predates the sighting"
    );

    // Still associated: later polls publish no new fix.
    no_fix_for(&phone_sub, 4).await;

    // Gone, then seen again only by the AP without a location: no fix.
    gw.control(&format!("/control/station?mac={PHONE_MAC}&present=false"));
    expect_state(&sighted_sub, json!(false)).await;
    ap.control(&format!("/control/station?mac={PHONE_MAC}&present=true"));
    expect_state(&sighted_sub, json!(true)).await;
    no_fix_for(&phone_sub, 3).await;

    // Gone again, then back at the gateway: a new, later fix.
    ap.control(&format!("/control/station?mac={PHONE_MAC}&present=false"));
    expect_state(&sighted_sub, json!(false)).await;
    gw.control(&format!("/control/station?mac={PHONE_MAC}&present=true"));
    let again = until_fix(&phone_sub).await;
    let later = again
        .last()
        .unwrap()
        .1
        .as_f64()
        .expect("fixed_at is a number");
    assert!(
        later > fixed_at,
        "second fix {later} not after the first {fixed_at}"
    );

    sup.shutdown();
}

/// (h) A device back in range before the away delay ran out gets a new fix,
/// though `sighted` never turned false. A restart gives none to a device
/// the mirror already holds as sighted: a router lists a station for
/// minutes after it leaves, so that fix could outdate the phone's own.
#[tokio::test(flavor = "multi_thread")]
async fn a_return_within_the_hold_is_a_fix_and_a_restart_is_not() {
    let gw = FakeOpenwrt::spawn().await;
    let routers = Scratch::new("openwrt-routers");
    let routers_path =
        routers_file_with(&routers, &[("gw", gw.port, "lat = 59.33\nlon = 18.06\n")]);
    let (mut sup, observer) = start(&routers_path).await;
    config_write(&observer, "home/config/openwrt/away_delay_s", json!(30))
        .await
        .expect("away delay write");
    let phone_sub = observer
        .declare_subscriber("home/state/person/dads_phone/*")
        .await
        .expect("phone subscriber");

    gw.control(&format!("/control/station?mac={PHONE_MAC}&present=true"));
    let first = until_fix(&phone_sub).await;
    assert!(
        first.contains(&("accuracy".to_string(), json!(50.0))),
        "default radius: {first:?}"
    );
    let first = first
        .last()
        .unwrap()
        .1
        .as_f64()
        .expect("fixed_at is a number");

    // Out of range for a few polls, well inside the 30 s delay.
    gw.control(&format!("/control/station?mac={PHONE_MAC}&present=false"));
    tokio::time::sleep(Duration::from_secs(3)).await;
    gw.control(&format!("/control/station?mac={PHONE_MAC}&present=true"));
    let back = until_fix(&phone_sub).await;
    assert!(
        !back.contains(&("sighted".to_string(), json!(false))),
        "sighted turned false inside the delay: {back:?}"
    );
    let back = back
        .last()
        .unwrap()
        .1
        .as_f64()
        .expect("fixed_at is a number");
    assert!(
        back > first,
        "return fix {back} not after the first {first}"
    );

    // The adapter restarted while the phone stays in range: no fix.
    restart_unit(&observer, "openwrt").await;
    // The new process's first cycle republishes `sighted`, and a fix would
    // come just before it.
    await_sample(
        &phone_sub,
        Duration::from_secs(20),
        "republished sighted",
        |msg| {
            assert!(
                !msg.key.ends_with("/fixed_at"),
                "a restart fixed the phone again"
            );
            msg.key.ends_with("/sighted")
        },
    )
    .await;
    no_fix_for(&phone_sub, 3).await;

    sup.shutdown();
}
