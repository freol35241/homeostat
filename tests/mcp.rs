//! Step-6 integration tests: the MCP agent surface against the real server
//! and a live supervised house. The stdio scenarios drive `homeostat mcp`
//! as a child process speaking newline-delimited JSON-RPC — the shape a
//! local MCP client launches; the HTTP scenario runs the server as a
//! supervised service unit, the deployed shape. Repo-editing scenarios run
//! on git-inited temp-dir copies of tests/fixture_house_apply/.
//!
//! No fixed ports, no wall-clock sleeps: every wait polls an observable
//! condition within a deadline.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio};
use std::time::{Duration, Instant};

use homeostat::bus::HealthStatus;
use serde_json::{json, Value};

use common::{await_health, free_port, health_watch, Supervisor};

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixture_house_apply")
}

/// A fresh editable copy of the fixture house in a temp dir.
fn temp_house(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("homeostat-mcp-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    copy_dir(&fixture(), &dir);
    dir
}

fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).expect("create dir");
    for entry in std::fs::read_dir(src).expect("read fixture dir") {
        let entry = entry.expect("dir entry");
        let target = dst.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy fixture file");
        }
    }
}

fn git(house: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(house)
        .args(["-c", "user.name=test", "-c", "user.email=test@example.com"])
        .args(args)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn git_init_commit(house: &Path) -> String {
    git(house, &["init", "-q", "-b", "main"]);
    git(house, &["add", "-A"]);
    git(house, &["commit", "-qm", "initial"]);
    git(house, &["rev-parse", "HEAD"])
}

/// Runs the homeostat CLI, returning its output.
fn cli(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_homeostat"))
        .args(args)
        .env_remove(homeostat::bus::ENV_BUS)
        .output()
        .expect("run homeostat CLI")
}

/// Reads a concrete key from a core queryable, decoding JSON.
async fn cache_read(session: &zenoh::Session, key: &str) -> Option<Value> {
    let replies = session.get(key).await.expect("cache read query");
    while let Ok(reply) = replies.recv_async().await {
        if let Ok(sample) = reply.result() {
            return Some(
                serde_json::from_slice(&sample.payload().to_bytes()).expect("reply is JSON"),
            );
        }
    }
    None
}

/// Reads a concrete key from a core queryable as a raw string (meta values
/// like commits are not JSON).
async fn meta_read(session: &zenoh::Session, key: &str) -> Option<String> {
    let replies = session.get(key).await.expect("meta read query");
    while let Ok(reply) = replies.recv_async().await {
        if let Ok(sample) = reply.result() {
            return Some(String::from_utf8_lossy(&sample.payload().to_bytes()).to_string());
        }
    }
    None
}

/// The unit's current pid per the health queryable; panics unless running.
async fn running_pid(session: &zenoh::Session, unit: &str) -> u64 {
    let health = cache_read(session, &homeostat::bus::health_key(unit))
        .await
        .unwrap_or_else(|| panic!("no health served for {unit}"));
    assert_eq!(health["status"], json!("running"), "{unit} health: {health}");
    health["pid"].as_u64().expect("running unit has a pid")
}

/// Waits until both fixture units are running and returns (probe pid,
/// reflector pid). Generous timeout: the first run resolves probe's uv env.
async fn await_base_units(session: &zenoh::Session) -> (u64, u64) {
    let mut probe = health_watch(session, "probe").await;
    await_health(&mut probe, Duration::from_secs(120), |h| {
        h.status == HealthStatus::Running
    })
    .await;
    let mut reflector = health_watch(session, "reflector").await;
    await_health(&mut reflector, Duration::from_secs(30), |h| {
        h.status == HealthStatus::Running
    })
    .await;
    (
        running_pid(session, "probe").await,
        running_pid(session, "reflector").await,
    )
}

/// An MCP client over the server's stdio transport: `homeostat mcp` as a
/// child process, one JSON-RPC line per request. `connect` performs the
/// initialize handshake.
struct Mcp {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<ChildStdout>,
    next_id: i64,
}

impl Mcp {
    fn connect(house: &Path, endpoint: &str) -> Mcp {
        let mut child = Command::new(env!("CARGO_BIN_EXE_homeostat"))
            .args(["mcp", house.to_str().expect("utf-8 path"), "--bus", endpoint])
            .env_remove(homeostat::bus::ENV_BUS)
            .env_remove(homeostat::bus::ENV_UNIT)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn mcp server");
        let stdin = child.stdin.take().expect("mcp stdin");
        let reader = BufReader::new(child.stdout.take().expect("mcp stdout"));
        let mut mcp = Mcp { child, stdin, reader, next_id: 0 };
        let init = mcp.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "homeostat-tests", "version": "0"}
            }),
        );
        assert_eq!(init["serverInfo"]["name"], json!("homeostat"), "{init}");
        let note = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        writeln!(mcp.stdin, "{note}").expect("write to mcp");
        mcp
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.stdin, "{message}").expect("write to mcp");
        self.stdin.flush().expect("flush to mcp");
        let mut line = String::new();
        self.reader.read_line(&mut line).expect("read from mcp");
        let reply: Value = serde_json::from_str(&line).expect("mcp reply is JSON");
        assert_eq!(reply["id"], json!(id), "{reply}");
        assert!(reply.get("error").is_none(), "mcp error reply: {reply}");
        reply["result"].clone()
    }

    /// Calls a tool, returning its text output and the isError flag.
    fn call(&mut self, tool: &str, args: Value) -> (String, bool) {
        let result = self.request("tools/call", json!({"name": tool, "arguments": args}));
        let text = result["content"][0]["text"]
            .as_str()
            .expect("text content")
            .to_string();
        (text, result["isError"].as_bool().expect("isError flag"))
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// (a) read_state serves live values through the core cache and
/// read_history returns what the recorder wrote, over the bus end to end.
#[tokio::test(flavor = "multi_thread")]
async fn reads_serve_live_state_and_history() {
    let db = std::env::temp_dir().join(format!("homeostat-mcp-history-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&db);
    let sup = Supervisor::spawn_with_env(
        "tests/fixture_house_recorder",
        &[("RECORDER_DB", db.to_str().expect("utf-8 path"))],
    );
    let observer = sup.observer().await;
    let mut recorder = health_watch(&observer, "recorder").await;
    await_health(&mut recorder, Duration::from_secs(120), |h| {
        h.status == HealthStatus::Running
    })
    .await;

    // Publish once a subscriber (the core state mirror, the recorder)
    // matches, so the put is never write-side filtered.
    let publisher = observer
        .declare_publisher("home/state/attic/mcp_probe/level")
        .await
        .expect("publisher");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let status = publisher.matching_status().await.expect("matching status");
        if status.matching() {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "no subscriber matched");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    publisher.put("7").await.expect("state put");

    let house = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixture_house_recorder");
    let mut mcp = Mcp::connect(&house, &sup.endpoint);

    // Live state via the core's last-value mirror.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (text, is_error) =
            mcp.call("read_state", json!({"key": "home/state/attic/mcp_probe/level"}));
        assert!(!is_error, "{text}");
        let values: Value = serde_json::from_str(&text).expect("read_state returns JSON");
        if values["home/state/attic/mcp_probe/level"] == json!(7) {
            break;
        }
        assert!(Instant::now() < deadline, "state never served: {text}");
        std::thread::sleep(Duration::from_millis(100));
    }

    // The same value lands in history and reads back over the bus.
    let deadline = Instant::now() + Duration::from_secs(30);
    let rows = loop {
        let (text, is_error) =
            mcp.call("read_history", json!({"series": "state/mcp_probe/level"}));
        assert!(!is_error, "{text}");
        let values: Value = serde_json::from_str(&text).expect("read_history returns JSON");
        let rows = values["home/history/state/mcp_probe/level"].clone();
        if rows.as_array().is_some_and(|r| !r.is_empty()) {
            break rows;
        }
        assert!(Instant::now() < deadline, "history never served: {text}");
        std::thread::sleep(Duration::from_millis(200));
    };
    let row = &rows[0];
    assert_eq!(row["value"], json!(7), "{rows}");
    assert_eq!(row["room"], json!("attic"), "{rows}");
    assert!(row["ts"].is_string(), "{rows}");

    // A malformed selector is an error result, not a crash.
    let (text, is_error) = mcp.call(
        "read_history",
        json!({"series": "state/mcp_probe/level", "from": 5}),
    );
    assert!(is_error, "{text}");

    drop(mcp);
    let mut sup = sup;
    sup.shutdown();
    let _ = std::fs::remove_file(&db);
}

/// (a') The deployed shape: the server runs as a supervised service unit
/// over HTTP — it declares the unit liveliness token (health `running`)
/// and answers MCP over POST.
#[tokio::test(flavor = "multi_thread")]
async fn http_transport_runs_as_supervised_unit() {
    let house = temp_house("http");
    let port = free_port();
    std::fs::write(
        house.join("units/mcp.toml"),
        format!(
            "schema = 1\n\n[unit]\nname = \"mcp\"\nkind = \"service\"\n\
             description = \"Agent surface\"\n\n\
             [runtime]\ncommand = \"homeostat mcp --http 127.0.0.1:{port}\"\n\
             restart = \"on-failure\"\nshutdown_grace_s = 5\n"
        ),
    )
    .expect("write mcp manifest");

    let mut sup = Supervisor::spawn_at(&house, &[]);
    let observer = sup.observer().await;
    await_base_units(&observer).await;
    let mut mcp_health = health_watch(&observer, "mcp").await;
    await_health(&mut mcp_health, Duration::from_secs(60), |h| {
        h.status == HealthStatus::Running
    })
    .await;

    let addr = format!("127.0.0.1:{port}");
    let (status, init) = http_post_retry(
        &addr,
        &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {"protocolVersion": "2025-06-18"}}),
        Duration::from_secs(10),
    );
    assert_eq!(status, 200);
    assert_eq!(init["result"]["serverInfo"]["name"], json!("homeostat"), "{init}");

    // A notification is accepted with no body.
    let (status, _) = http_post_retry(
        &addr,
        &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        Duration::from_secs(5),
    );
    assert_eq!(status, 202);

    let (status, reply) = http_post_retry(
        &addr,
        &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": {"name": "read_state",
                           "arguments": {"key": "home/health/probe"}}}),
        Duration::from_secs(5),
    );
    assert_eq!(status, 200);
    let text = reply["result"]["content"][0]["text"]
        .as_str()
        .expect("text content");
    assert!(text.contains("\"status\": \"running\""), "{text}");

    // Supervisor shutdown also proves the unit exits cleanly within grace.
    sup.shutdown();
    let _ = std::fs::remove_dir_all(&house);
}

/// One HTTP POST: connect, send, read the full response. Retries while the
/// server's listener may still be coming up.
fn http_post_retry(addr: &str, message: &Value, timeout: Duration) -> (u16, Value) {
    let deadline = Instant::now() + timeout;
    loop {
        match http_post(addr, message) {
            Ok(reply) => return reply,
            Err(err) => {
                assert!(Instant::now() < deadline, "POST to {addr} failed: {err}");
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

fn http_post(addr: &str, message: &Value) -> Result<(u16, Value), String> {
    let body = message.to_string();
    let mut stream = TcpStream::connect(addr).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .map_err(|e| e.to_string())?;
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n\
         Accept: application/json, text/event-stream\r\nConnection: close\r\n\
         Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).map_err(|e| e.to_string())?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).map_err(|e| e.to_string())?;
    let response = String::from_utf8_lossy(&response).to_string();
    let status: u16 = response
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("no status line in {response:?}"))?;
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .unwrap_or("");
    let value = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(body).map_err(|e| format!("body is not JSON: {e}: {body:?}"))?
    };
    Ok((status, value))
}

/// (b) A parameter propose within constraints auto-applies: the commit
/// lands, the running unit sees the value with no restart.
#[tokio::test(flavor = "multi_thread")]
async fn parameter_propose_commits_and_auto_applies() {
    let house = temp_house("param");
    git_init_commit(&house);
    let mut sup = Supervisor::spawn_at(&house, &[]);
    let observer = sup.observer().await;
    let (probe_pid, reflector_pid) = await_base_units(&observer).await;

    let manifest = std::fs::read_to_string(house.join("units/probe.toml")).expect("read manifest");
    let mut mcp = Mcp::connect(&house, &sup.endpoint);
    let (text, is_error) = mcp.call(
        "propose",
        json!({
            "files": [{"path": "units/probe.toml",
                       "content": manifest.replace("default = 1", "default = 5")}],
            "message": "raise level default to 5"
        }),
    );
    assert!(!is_error, "{text}");
    assert!(text.contains("Plan tier: parameter-only"), "{text}");
    assert!(text.contains("parameter probe/level = 5"), "{text}");
    assert!(text.contains("Applied."), "{text}");

    assert_eq!(
        cache_read(&observer, "home/config/probe/level").await,
        Some(json!(5)),
        "the live value follows the repo"
    );
    assert_eq!(running_pid(&observer, "probe").await, probe_pid, "zero restarts");
    assert_eq!(running_pid(&observer, "reflector").await, reflector_pid);

    // The commit landed on the current branch and the tree is clean.
    let head = git(&house, &["rev-parse", "HEAD"]);
    assert!(text.contains(&format!("Committed {head}")), "{text}");
    assert_eq!(git(&house, &["status", "--porcelain"]), "");
    assert_eq!(
        git(&house, &["log", "-1", "--format=%s"]),
        "raise level default to 5"
    );
    assert_eq!(
        meta_read(&observer, homeostat::bus::APPLIED_COMMIT_KEY).await,
        Some(head),
        "applied_commit advances to the agent's commit"
    );

    drop(mcp);
    sup.shutdown();
    let _ = std::fs::remove_dir_all(&house);
}

/// (c) An out-of-constraint parameter propose is rejected with the
/// constraint named; repo and world unchanged, nothing committed.
#[tokio::test(flavor = "multi_thread")]
async fn out_of_constraint_propose_is_rejected_and_reverted() {
    let house = temp_house("reject");
    let base = git_init_commit(&house);
    let mut sup = Supervisor::spawn_at(&house, &[]);
    let observer = sup.observer().await;
    await_base_units(&observer).await;

    let manifest = std::fs::read_to_string(house.join("units/probe.toml")).expect("read manifest");
    let mut mcp = Mcp::connect(&house, &sup.endpoint);
    let (text, is_error) = mcp.call(
        "propose",
        json!({
            "files": [{"path": "units/probe.toml",
                       "content": manifest.replace("default = 1", "default = 50")}],
            "message": "way too high"
        }),
    );
    assert!(is_error, "an out-of-constraint default must be an error: {text}");
    assert!(text.contains("50 is above max 10"), "the constraint is named: {text}");

    // Repo unchanged: the file is restored, nothing was committed.
    let on_disk = std::fs::read_to_string(house.join("units/probe.toml")).expect("read manifest");
    assert!(on_disk.contains("default = 1"), "{on_disk}");
    assert_eq!(git(&house, &["rev-parse", "HEAD"]), base);
    assert_eq!(git(&house, &["status", "--porcelain"]), "");

    // World unchanged: the old value still drives behavior.
    assert_eq!(
        cache_read(&observer, "home/config/probe/level").await,
        Some(json!(1))
    );

    drop(mcp);
    sup.shutdown();
    let _ = std::fs::remove_dir_all(&house);
}

const BEACON_ADAPTER: &str = "schema = 1\n\n[unit]\nname = \"beacon\"\nkind = \"adapter\"\n\n\
    [runtime]\ncommand = \"fake_adapter\"\nrestart = \"on-failure\"\n\n\
    [discovery]\nmode = \"static\"\nendpoint = \"fake://local\"\n\n\
    [entities]\ndir = \"entities/beacon/\"\n";

const BEACON_LAMP: &str = "schema = 1\n\n[entity]\nid = \"beacon-1\"\ncapability = \"light\"\n\
    room = \"den\"\n\n[write_policy]\nmode = \"shared\"\nowner = \"beacon\"\n";

const WATCHER: &str = "schema = 1\n\n[unit]\nname = \"watcher\"\nkind = \"automation\"\n\n\
    [runtime]\ncommand = \"reflector\"\nrestart = \"on-failure\"\n\n\
    [bus.publishes]\nlights = { key = \"home/cmd/den/beacon_lamp/on\", \
    capability = \"light\", priority = \"agent\" }\n";

/// (d) A structural propose — a new adapter plus an automation granted onto
/// its entity — lands committed as a pending plan and does not touch the
/// world; the agent's own apply is refused; the owner applies the pending
/// plan and the walk runs in grant order.
#[tokio::test(flavor = "multi_thread")]
async fn structural_propose_awaits_owner_approval() {
    let house = temp_house("structural");
    git_init_commit(&house);
    let mut sup = Supervisor::spawn_at(&house, &[]);
    let observer = sup.observer().await;
    let (probe_pid, _) = await_base_units(&observer).await;

    let mut mcp = Mcp::connect(&house, &sup.endpoint);
    let (text, is_error) = mcp.call(
        "propose",
        json!({
            "files": [
                {"path": "units/beacon.toml", "content": BEACON_ADAPTER},
                {"path": "entities/beacon/beacon_lamp.toml", "content": BEACON_LAMP},
                {"path": "units/watcher.toml", "content": WATCHER}
            ],
            "message": "add beacon adapter and watcher automation"
        }),
    );
    assert!(!is_error, "{text}");
    assert!(text.contains("Plan tier: structural"), "{text}");
    assert!(
        text.contains("+ watcher.lights  capability=light  priority=agent"),
        "the grant diff is rendered: {text}"
    );
    assert!(text.contains("owner approval required"), "{text}");
    let plan_path = text
        .lines()
        .find_map(|l| l.strip_prefix("Pending plan saved: "))
        .expect("pending plan path in the response")
        .to_string();
    assert!(Path::new(&plan_path).is_file(), "{plan_path}");

    // The world is untouched: the proposed units do not exist.
    assert_eq!(
        cache_read(&observer, &homeostat::bus::health_key("beacon")).await,
        None
    );
    assert_eq!(
        cache_read(&observer, &homeostat::bus::health_key("watcher")).await,
        None
    );

    // The agent's own apply is refused at tier.
    let (text, is_error) = mcp.call("apply", json!({}));
    assert!(is_error, "{text}");
    assert!(text.contains("apply refused at agent tier"), "{text}");
    assert!(text.contains("structural"), "{text}");

    // The owner applies the pending plan; the walk runs adapter first.
    let house_arg = house.to_str().expect("utf-8 path");
    let apply = cli(&["apply", house_arg, "--bus", &sup.endpoint, "--plan", &plan_path]);
    let out = String::from_utf8_lossy(&apply.stdout).to_string();
    assert!(
        apply.status.success(),
        "owner apply failed\nstdout:\n{out}\nstderr:\n{}",
        String::from_utf8_lossy(&apply.stderr)
    );
    let beacon_at = out.find("start beacon: ok").expect("beacon started");
    let watcher_at = out.find("start watcher: ok").expect("watcher started");
    assert!(beacon_at < watcher_at, "grant order:\n{out}");

    assert!(running_pid(&observer, "beacon").await > 0);
    assert!(running_pid(&observer, "watcher").await > 0);
    assert_eq!(running_pid(&observer, "probe").await, probe_pid, "untouched");

    drop(mcp);
    sup.shutdown();
    let _ = std::fs::remove_dir_all(&house);
}

/// (e) The smuggling test: a manifest edit that changes a default AND adds
/// a publish (a grant delta) escalates to structural through the MCP
/// surface — the mechanical tier derivation is the enforcement, so nothing
/// applies and the live value stands.
#[tokio::test(flavor = "multi_thread")]
async fn grant_delta_in_manifest_edit_escalates_to_structural() {
    let house = temp_house("smuggle");
    git_init_commit(&house);
    let mut sup = Supervisor::spawn_at(&house, &[]);
    let observer = sup.observer().await;
    let (probe_pid, _) = await_base_units(&observer).await;

    let manifest = std::fs::read_to_string(house.join("units/probe.toml")).expect("read manifest");
    let smuggled = manifest
        .replace("default = 1", "default = 2")
        .replace(
            "echo = { key = \"home/state/den/probe_echo/level\" }",
            "echo = { key = \"home/state/den/probe_echo/level\" }\n\
             lights = { key = \"home/cmd/livingroom/lamp/on\", \
             capability = \"light\", priority = \"agent\" }",
        );
    assert_ne!(smuggled, manifest, "the fixture manifest changed shape");

    let mut mcp = Mcp::connect(&house, &sup.endpoint);
    let (text, is_error) = mcp.call(
        "propose",
        json!({
            "files": [{"path": "units/probe.toml", "content": smuggled}],
            "message": "just a parameter tweak (and a grant)"
        }),
    );
    assert!(!is_error, "{text}");
    assert!(
        text.contains("Plan tier: structural"),
        "a grant delta escalates the whole plan: {text}"
    );
    assert!(!text.contains("Plan tier: parameter-only"), "{text}");
    assert!(
        text.contains("+ probe.lights  capability=light  priority=agent"),
        "{text}"
    );
    assert!(text.contains("Pending plan saved: "), "{text}");

    // Nothing applied: the live value stands, the unit was not restarted.
    assert_eq!(
        cache_read(&observer, "home/config/probe/level").await,
        Some(json!(1)),
        "the smuggled default did not land"
    );
    assert_eq!(running_pid(&observer, "probe").await, probe_pid, "no restart");

    drop(mcp);
    sup.shutdown();
    let _ = std::fs::remove_dir_all(&house);
}
