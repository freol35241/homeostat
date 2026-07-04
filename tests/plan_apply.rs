//! Step-5b integration tests: plan/apply against a live world, on temp-dir
//! copies of tests/fixture_house_apply/ that each scenario edits between
//! plan and apply. Scenarios that need applied_commit or pending-plan
//! staleness git-init their copy: the checked-in fixture is a nested
//! directory of this repo and must not inherit its HEAD (see docs/design.md).
//!
//! No fixed ports (each supervisor gets a fresh ephemeral endpoint), no
//! wall-clock sleeps (every wait polls a bus-observable condition within a
//! deadline).

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use homeostat::bus::HealthStatus;
use serde_json::{json, Value};

use common::{await_health, health_watch, Supervisor};

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixture_house_apply")
}

/// A fresh editable copy of the fixture house in a temp dir.
fn temp_house(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "homeostat-apply-{tag}-{}",
        std::process::id()
    ));
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

/// Replaces `from` with `to` in a house file; the pattern must be present.
fn edit(house: &Path, rel: &str, from: &str, to: &str) {
    let path = house.join(rel);
    let text = std::fs::read_to_string(&path).expect("read house file");
    assert!(text.contains(from), "{rel} does not contain {from:?}");
    std::fs::write(&path, text.replace(from, to)).expect("write house file");
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
    git_commit_all(house, "initial")
}

fn git_commit_all(house: &Path, message: &str) -> String {
    git(house, &["add", "-A"]);
    git(house, &["commit", "-qm", message]);
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

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn assert_cli_ok(output: &Output) {
    assert!(
        output.status.success(),
        "CLI failed\nstdout:\n{}\nstderr:\n{}",
        stdout(output),
        stderr(output)
    );
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
/// like hashes and commits are not JSON).
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

/// (a) A behavioral change — the automation's code edited — plans as
/// behavioral and apply restarts exactly that unit: the adapter's pid
/// survives, and applied_commit updates to the repo's HEAD.
#[tokio::test(flavor = "multi_thread")]
async fn behavioral_change_restarts_exactly_that_unit() {
    let house = temp_house("behavioral");
    git_init_commit(&house);
    let mut sup = Supervisor::spawn_at(&house, &[]);
    let observer = sup.observer().await;
    let (probe_pid, reflector_pid) = await_base_units(&observer).await;

    edit(&house, "units/probe.py", "MARKER v1", "MARKER v2");
    let head = git_commit_all(&house, "probe v2");

    let house_arg = house.to_str().expect("utf-8 path");
    let plan = cli(&["plan", house_arg, "--bus", &sup.endpoint]);
    assert_cli_ok(&plan);
    let text = stdout(&plan);
    assert!(text.contains("Plan tier: behavioral (1 unit restarted)"), "{text}");
    assert!(text.contains("~ automation probe (units/probe.toml)"), "{text}");
    assert!(text.contains("reason: unit files changed"), "{text}");

    let apply = cli(&["apply", house_arg, "--bus", &sup.endpoint]);
    assert_cli_ok(&apply);
    let text = stdout(&apply);
    assert!(text.contains("restart probe: ok"), "{text}");
    assert!(text.contains("Applied."), "{text}");

    let new_probe_pid = running_pid(&observer, "probe").await;
    assert_ne!(new_probe_pid, probe_pid, "probe restarted");
    assert_eq!(
        running_pid(&observer, "reflector").await,
        reflector_pid,
        "the adapter's pid survives a behavioral change elsewhere"
    );
    assert_eq!(
        meta_read(&observer, homeostat::bus::APPLIED_COMMIT_KEY).await,
        Some(head),
        "applied_commit readable from the bus"
    );

    sup.shutdown();
    let _ = std::fs::remove_dir_all(&house);
}

/// (b) A manifest-default change plans as parameter-only and applies with
/// zero restarts; the running unit sees the new value live (it echoes the
/// config update to a state key).
#[tokio::test(flavor = "multi_thread")]
async fn parameter_default_change_applies_with_zero_restarts() {
    let house = temp_house("parameter");
    let mut sup = Supervisor::spawn_at(&house, &[]);
    let observer = sup.observer().await;
    let echo_sub = observer
        .declare_subscriber("home/state/den/probe_echo/level")
        .await
        .expect("echo subscriber");
    let (probe_pid, reflector_pid) = await_base_units(&observer).await;

    edit(&house, "units/probe.toml", "default = 1", "default = 5");

    let house_arg = house.to_str().expect("utf-8 path");
    let plan = cli(&["plan", house_arg, "--bus", &sup.endpoint]);
    assert_cli_ok(&plan);
    let text = stdout(&plan);
    assert!(
        text.contains("Plan tier: parameter-only (1 parameter change)"),
        "{text}"
    );
    assert!(text.contains("~ probe/level  live=1  repo=5"), "{text}");

    let apply = cli(&["apply", house_arg, "--bus", &sup.endpoint]);
    assert_cli_ok(&apply);
    let text = stdout(&apply);
    assert!(text.contains("parameter probe/level = 5"), "{text}");
    assert!(!text.contains("restart"), "no restart steps: {text}");

    // The running unit echoes the new value: it saw the config put live.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let sample = tokio::time::timeout_at(deadline, echo_sub.recv_async())
            .await
            .expect("echo of the new value within 10s")
            .expect("echo stream open");
        let value: Value = serde_json::from_slice(&sample.payload().to_bytes())
            .expect("echo payload is JSON");
        if value == json!(5) {
            break;
        }
    }

    assert_eq!(running_pid(&observer, "probe").await, probe_pid, "zero restarts");
    assert_eq!(running_pid(&observer, "reflector").await, reflector_pid);
    assert_eq!(
        cache_read(&observer, "home/config/probe/level").await,
        Some(json!(5)),
        "the core cache serves the repo value"
    );

    sup.shutdown();
    let _ = std::fs::remove_dir_all(&house);
}

fn add_watcher_pair(house: &Path, adapter: &str, room: &str, entity: &str, automation: &str) {
    std::fs::write(
        house.join(format!("units/{adapter}.toml")),
        format!(
            "schema = 1\n\n[unit]\nname = \"{adapter}\"\nkind = \"adapter\"\n\n\
             [runtime]\ncommand = \"fake_adapter --crash-after-ms 0\"\nrestart = \"on-failure\"\n\n\
             [discovery]\nmode = \"static\"\nendpoint = \"fake://local\"\n\n\
             [entities]\ndir = \"entities/{adapter}/\"\n"
        ),
    )
    .expect("write adapter manifest");
    std::fs::create_dir_all(house.join(format!("entities/{adapter}"))).expect("entities dir");
    std::fs::write(
        house.join(format!("entities/{adapter}/{entity}.toml")),
        format!(
            "schema = 1\n\n[entity]\nid = \"{entity}-1\"\ncapability = \"light\"\nroom = \"{room}\"\n\n\
             [write_policy]\nmode = \"shared\"\nowner = \"{adapter}\"\n"
        ),
    )
    .expect("write entity file");
    std::fs::write(
        house.join(format!("units/{automation}.toml")),
        format!(
            "schema = 1\n\n[unit]\nname = \"{automation}\"\nkind = \"automation\"\n\n\
             [runtime]\ncommand = \"reflector\"\nrestart = \"on-failure\"\n\n\
             [bus.publishes]\nlights = {{ key = \"home/cmd/{room}/{entity}/on\", capability = \"light\", priority = \"automation\" }}\n"
        ),
    )
    .expect("write automation manifest");
}

/// (c) A structural change — a new adapter plus a new automation granted
/// onto its entity — plans as structural with the grant diff rendered, and
/// apply starts units in grant order: the adapter before the dependent
/// automation. Untouched units keep their pids.
#[tokio::test(flavor = "multi_thread")]
async fn structural_change_starts_units_in_grant_order() {
    let house = temp_house("structural");
    let mut sup = Supervisor::spawn_at(&house, &[]);
    let observer = sup.observer().await;
    let (probe_pid, reflector_pid) = await_base_units(&observer).await;

    add_watcher_pair(&house, "beacon", "den", "beacon_lamp", "watcher");
    // The beacon must actually come up for the walk to proceed.
    edit(&house, "units/beacon.toml", "fake_adapter --crash-after-ms 0", "fake_adapter");

    let house_arg = house.to_str().expect("utf-8 path");
    let plan = cli(&["plan", house_arg, "--bus", &sup.endpoint]);
    assert_cli_ok(&plan);
    let text = stdout(&plan);
    assert!(
        text.contains("Plan tier: structural (2 units created, 1 grant added)"),
        "{text}"
    );
    assert!(text.contains("Grant changes:"), "{text}");
    assert!(
        text.contains("+ watcher.lights  capability=light  priority=automation"),
        "the grant diff is rendered: {text}"
    );
    assert!(text.contains("-> beacon_lamp"), "{text}");

    let apply = cli(&["apply", house_arg, "--bus", &sup.endpoint]);
    assert_cli_ok(&apply);
    let text = stdout(&apply);
    let beacon_at = text.find("start beacon: ok").expect("beacon started");
    let watcher_at = text.find("start watcher: ok").expect("watcher started");
    assert!(
        beacon_at < watcher_at,
        "adapter starts before the dependent automation:\n{text}"
    );

    assert!(running_pid(&observer, "beacon").await > 0);
    assert!(running_pid(&observer, "watcher").await > 0);
    assert_eq!(running_pid(&observer, "probe").await, probe_pid, "untouched");
    assert_eq!(running_pid(&observer, "reflector").await, reflector_pid, "untouched");

    sup.shutdown();
    let _ = std::fs::remove_dir_all(&house);
}

/// (d) A unit that fails to become ready mid-walk halts the apply in
/// place and reports position: earlier units keep running, later units
/// are never started, and a re-plan shows exactly the remaining work.
#[tokio::test(flavor = "multi_thread")]
async fn failing_unit_halts_walk_in_place() {
    let house = temp_house("halt");
    let mut sup = Supervisor::spawn_at(&house, &[]);
    let observer = sup.observer().await;
    let (probe_pid, reflector_pid) = await_base_units(&observer).await;

    // The broken adapter crashes on spawn until its breaker opens; the
    // waiter automation is granted onto its entity, so it walks second.
    add_watcher_pair(&house, "broken", "attic", "broken_light", "waiter");

    let house_arg = house.to_str().expect("utf-8 path");
    let apply = cli(&["apply", house_arg, "--bus", &sup.endpoint]);
    assert!(!apply.status.success(), "apply reports failure");
    let out = stdout(&apply);
    let err = stderr(&apply);
    assert!(out.contains("start broken: FAILED (circuit breaker open)"), "{out}");
    assert!(err.contains("apply halted at broken"), "{err}");
    assert!(err.contains("not reached: waiter"), "{err}");

    // Bus state after the halt: the failed unit's breaker is visible,
    // the never-reached unit does not exist, earlier units still run.
    let health = cache_read(&observer, &homeostat::bus::health_key("broken"))
        .await
        .expect("broken unit's health served");
    assert_eq!(health["status"], json!("open"), "{health}");
    assert_eq!(
        cache_read(&observer, &homeostat::bus::health_key("waiter")).await,
        None,
        "the walk never reached waiter"
    );
    assert_eq!(running_pid(&observer, "probe").await, probe_pid);
    assert_eq!(running_pid(&observer, "reflector").await, reflector_pid);
    assert_eq!(
        meta_read(&observer, homeostat::bus::APPLIED_COMMIT_KEY).await,
        None,
        "applied_commit does not advance on a halted walk"
    );

    // A re-plan shows exactly the remaining work.
    let plan = cli(&["plan", house_arg, "--bus", &sup.endpoint]);
    assert_cli_ok(&plan);
    let text = stdout(&plan);
    assert!(
        text.contains("Plan tier: structural (2 units created, 1 grant added)"),
        "the halted walk left the remaining work planned: {text}"
    );

    sup.shutdown();
    let _ = std::fs::remove_dir_all(&house);
}

/// (e) A pending plan whose base commit is stale refuses to apply.
#[tokio::test(flavor = "multi_thread")]
async fn stale_pending_plan_refuses_to_apply() {
    let house = temp_house("stale");
    git_init_commit(&house);
    let mut sup = Supervisor::spawn_at(&house, &[]);
    let observer = sup.observer().await;
    await_base_units(&observer).await;

    edit(&house, "units/probe.toml", "default = 1", "default = 7");
    git_commit_all(&house, "raise level default");

    let house_arg = house.to_str().expect("utf-8 path");
    let saved = cli(&["plan", house_arg, "--bus", &sup.endpoint, "--save"]);
    assert_cli_ok(&saved);
    let text = stdout(&saved);
    assert!(text.contains("Pending plan saved: "), "{text}");
    let plan_path = text
        .lines()
        .find_map(|l| l.strip_prefix("Pending plan saved: "))
        .expect("saved path printed")
        .to_string();

    // The repo moves past the plan's base commit: auto-invalidation.
    std::fs::write(house.join("NOTES.md"), "repo moved on\n").expect("write file");
    git_commit_all(&house, "unrelated change");

    let apply = cli(&["apply", house_arg, "--bus", &sup.endpoint, "--plan", &plan_path]);
    assert!(!apply.status.success(), "stale plan must refuse");
    let err = stderr(&apply);
    assert!(err.contains("stale"), "{err}");

    // Nothing was applied: the live value is still the old default.
    assert_eq!(
        cache_read(&observer, "home/config/probe/level").await,
        Some(json!(1))
    );

    sup.shutdown();
    let _ = std::fs::remove_dir_all(&house);
}
