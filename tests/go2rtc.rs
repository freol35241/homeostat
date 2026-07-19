//! go2rtc shim integration tests: the first foreign binary as a unit. A
//! fake go2rtc (tests/fake_go2rtc.py) sits on PATH under the real binary
//! name via a wrapper script; the shim must render its config from
//! HOMEOSTAT_CAMERAS, spawn it, poll its API, own the liveliness token,
//! and translate child death into its own exit (supervisor restart).

mod common;

use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::Value;
use zenoh::sample::SampleKind;

use common::{free_port, Supervisor};

const FIXTURE: &str = "tests/fixture_house_go2rtc";
const CAMERAS_ENV: &str = "HOMEOSTAT_CAMERAS";
const LISTEN_ENV: &str = "HOMEOSTAT_GO2RTC_LISTEN";

/// A temp dir whose `go2rtc` executable is a wrapper around the fake
/// (tests/fake_go2rtc.py) — prepended to PATH so the shim's bare-name
/// spawn resolves to it, exactly as image provisioning would.
fn fake_binary_dir() -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("homeostat-go2rtc-bin-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create fake binary dir");
    let wrapper = dir.join("go2rtc");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nexec uv run {}/tests/fake_go2rtc.py \"$@\"\n",
            env!("CARGO_MANIFEST_DIR")
        ),
    )
    .expect("write go2rtc wrapper");
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755))
        .expect("chmod go2rtc wrapper");
    dir
}

fn cameras_file(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("homeostat-go2rtc-cameras-{tag}.toml"));
    std::fs::write(
        &path,
        "[porch_cam]\nhost = \"192.0.2.7:2020\"\nusername = \"homeostat\"\npassword = \"secret123\"\n",
    )
    .expect("write cameras file");
    path
}

/// Raw HTTP GET against the fake go2rtc's API.
fn api_get(port: u16, path: &str) -> Option<(u16, Value)> {
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    let status: u16 = response.split_whitespace().nth(1)?.parse().ok()?;
    let body = response.split("\r\n\r\n").nth(1)?;
    // aiohttp answers chunked on HTTP/1.1: strip chunk framing lines.
    let json_line = body.lines().find(|l| l.trim_start().starts_with('{'))?;
    Some((status, serde_json::from_str(json_line).ok()?))
}

fn control_quit(port: u16) {
    let mut stream =
        std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect for control quit");
    stream
        .write_all(
            b"POST /control/quit HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .expect("write control quit");
    let mut sink = String::new();
    let _ = stream.read_to_string(&mut sink); // the process exits mid-response
}

/// The whole shim contract in one walk: config rendered from the
/// credentials file (stream per camera, named by entity id, HD main
/// stream on 554), liveliness only after the API answers, child death ->
/// restart with a fresh incarnation, clean shutdown closes the listener.
#[tokio::test(flavor = "multi_thread")]
async fn shim_owns_the_token_for_the_foreign_binary() {
    let api_port = free_port();
    let listen = format!("127.0.0.1:{api_port}");
    let cameras_path = cameras_file(&api_port.to_string());
    let bin_dir = fake_binary_dir();
    let path_env = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").expect("PATH set")
    );

    let mut sup = Supervisor::spawn_with_env(
        FIXTURE,
        &[
            (CAMERAS_ENV, cameras_path.to_str().expect("utf-8 path")),
            (LISTEN_ENV, &listen),
            ("PATH", &path_env),
        ],
    );
    let observer = sup.observer().await;
    let token_sub = observer
        .liveliness()
        .declare_subscriber("home/health/go2rtc/alive")
        .history(true)
        .await
        .expect("liveliness subscriber");
    let token = tokio::time::timeout(Duration::from_secs(90), token_sub.recv_async())
        .await
        .expect("shim liveliness token within 90s")
        .expect("liveliness stream open");
    assert_eq!(token.kind(), SampleKind::Put);

    // The token implies the API answered; the rendered config carries one
    // stream per camera, named by entity id, on the RTSP default.
    let (status, streams) = api_get(api_port, "/api/streams").expect("api answers");
    assert_eq!(status, 200);
    assert_eq!(
        streams["porch_cam"]["producers"][0]["url"],
        Value::String("rtsp://homeostat:secret123@192.0.2.7:554/stream1".into()),
        "{streams}"
    );

    // Child death: the shim exits, the supervisor restarts, a fresh
    // incarnation re-declares the token and re-answers.
    control_quit(api_port);
    let dropped = tokio::time::timeout(Duration::from_secs(30), token_sub.recv_async())
        .await
        .expect("token drop within 30s of child death")
        .expect("liveliness stream open");
    assert_eq!(dropped.kind(), SampleKind::Delete);
    let restored = tokio::time::timeout(Duration::from_secs(60), token_sub.recv_async())
        .await
        .expect("token restored within 60s (restart)")
        .expect("liveliness stream open");
    assert_eq!(restored.kind(), SampleKind::Put);
    let (status, _) = api_get(api_port, "/api/streams").expect("api answers after restart");
    assert_eq!(status, 200);

    // Clean shutdown: no orphaned go2rtc holding the port.
    sup.shutdown();
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::net::TcpStream::connect(("127.0.0.1", api_port)).is_ok() {
        assert!(Instant::now() < deadline, "fake go2rtc survived supervisor shutdown");
        std::thread::sleep(Duration::from_millis(100));
    }
}
