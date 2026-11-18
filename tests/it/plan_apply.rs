//! Integration tests for plan/apply against a live world, on temp-dir
//! copies of `tests/fixtures/house_apply/` that each scenario edits between
//! plan and apply. Scenarios that need `applied_commit` or pending-plan
//! staleness git-init their copy: the checked-in fixture is a nested
//! directory of this repo and must not inherit its HEAD (see
//! docs/design.md#the-apply-walk).
//!
//! Each supervisor gets a fresh ephemeral endpoint, and every wait polls a
//! bus-observable condition within a deadline instead of sleeping.

use std::path::Path;
use std::time::Duration;

use serde_json::{json, Value};

use crate::common::{
    assert_cli_ok, await_sample, cache_read, cli, config_write, edit, git_commit_all,
    git_init_commit, meta_read, running_pid, stderr, stdout, temp_house, up, StateSub, Supervisor,
};

const FIXTURE: &str = "tests/fixtures/house_apply";

/// (a) A behavioral change (the automation's code edited) plans as
/// behavioral and apply restarts only that unit: the adapter's pid
/// survives, and `applied_commit` updates to the repo's HEAD.
#[tokio::test(flavor = "multi_thread")]
async fn behavioral_change_restarts_exactly_that_unit() {
    let house = temp_house(FIXTURE, "apply-behavioral");
    git_init_commit(&house);
    let (mut sup, observer) = up(
        || Supervisor::spawn_at(&house, &[]),
        &["probe", "reflector"],
    )
    .await;
    let probe_pid = running_pid(&observer, "probe").await;
    let reflector_pid = running_pid(&observer, "reflector").await;

    edit(&house, "units/probe.py", "MARKER v1", "MARKER v2");
    let head = git_commit_all(&house, "probe v2");

    let house_arg = house.to_str().expect("utf-8 path");
    let plan = cli(&["plan", house_arg, "--bus", &sup.endpoint]);
    assert_cli_ok(&plan);
    let text = stdout(&plan);
    assert!(
        text.contains("Plan tier: behavioral (1 unit restarted)"),
        "{text}"
    );
    assert!(
        text.contains("~ automation probe (units/probe.toml)"),
        "{text}"
    );
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
        Some(head.clone()),
        "applied_commit readable from the bus"
    );
    // The same facts as one document, for a surface that says where it is
    // served from (the dashboard's footer, an agent).
    let about: Value = serde_json::from_str(
        &meta_read(&observer, homeostat::bus::ABOUT_KEY)
            .await
            .expect("about readable from the bus"),
    )
    .expect("about is JSON");
    assert_eq!(about["house"]["commit"], json!(head), "{about}");
    assert_eq!(
        about["homeostat"]["version"],
        json!(env!("CARGO_PKG_VERSION")),
        "{about}"
    );

    sup.shutdown();
}

/// (a2) A manifest edit that adds a publish is behavioral (the grant set is
/// unchanged), and the plan renders the restarting unit's expanded keys so
/// the new key surface is visible before approval, as it is for a create.
#[tokio::test(flavor = "multi_thread")]
async fn manifest_edit_adding_a_publish_renders_the_new_key() {
    let house = temp_house(FIXTURE, "apply-new-key");
    let (mut sup, _observer) = up(
        || Supervisor::spawn_at(&house, &[]),
        &["probe", "reflector"],
    )
    .await;

    edit(
        &house,
        "units/probe.toml",
        "echo = { key = \"home/state/den/probe_echo/level\" }",
        "echo = { key = \"home/state/den/probe_echo/level\" }\n\
         events = { key = \"home/health/probe/event\" }",
    );

    let house_arg = house.to_str().expect("utf-8 path");
    let plan = cli(&["plan", house_arg, "--bus", &sup.endpoint]);
    assert_cli_ok(&plan);
    let text = stdout(&plan);
    assert!(
        text.contains("Plan tier: behavioral (1 unit restarted)"),
        "{text}"
    );
    assert!(text.contains("reason: manifest changed"), "{text}");
    assert!(text.contains("Expanded keys:"), "{text}");
    assert!(
        text.contains("probe publishes events: home/health/probe/event"),
        "{text}"
    );

    sup.shutdown();
}

/// (b) A manifest-default change plans as parameter-only and applies with
/// zero restarts; the running unit sees the new value live (it echoes the
/// config update to a state key).
#[tokio::test(flavor = "multi_thread")]
async fn parameter_default_change_applies_with_zero_restarts() {
    let house = temp_house(FIXTURE, "apply-parameter");
    let (mut sup, observer) = up(
        || Supervisor::spawn_at(&house, &[]),
        &["probe", "reflector"],
    )
    .await;
    let echo_sub = observer
        .declare_subscriber("home/state/den/probe_echo/level")
        .await
        .expect("echo subscriber");
    let probe_pid = running_pid(&observer, "probe").await;
    let reflector_pid = running_pid(&observer, "reflector").await;

    edit(&house, "units/probe.toml", "default = 1", "default = 5");

    let house_arg = house.to_str().expect("utf-8 path");
    let plan = cli(&["plan", house_arg, "--bus", &sup.endpoint]);
    assert_cli_ok(&plan);
    let text = stdout(&plan);
    assert!(
        text.contains("Plan tier: parameter-only (1 parameter change, 1 manifest refreshed)"),
        "{text}"
    );
    assert!(text.contains("~ probe/level  live=1  repo=5"), "{text}");
    assert!(text.contains("level: default 1 -> 5"), "{text}");

    let apply = cli(&["apply", house_arg, "--bus", &sup.endpoint]);
    assert_cli_ok(&apply);
    let text = stdout(&apply);
    assert!(text.contains("parameter probe/level = 5"), "{text}");
    assert!(!text.contains("restart"), "no restart steps: {text}");

    // The running unit echoes the new value: it saw the config put live.
    await_echo(&echo_sub, json!(5)).await;

    assert_eq!(
        running_pid(&observer, "probe").await,
        probe_pid,
        "zero restarts"
    );
    assert_eq!(running_pid(&observer, "reflector").await, reflector_pid);
    assert_eq!(
        cache_read(&observer, "home/config/probe/level").await,
        Some(json!(5)),
        "the core cache serves the repo value"
    );

    sup.shutdown();
}

/// (b2) A constraint-only manifest edit plans as parameter-only with the
/// delta rendered, and apply takes effect live with zero restarts: the
/// running store enforces the tightened constraint and the served meta
/// manifest updates, so a re-plan shows no changes.
#[tokio::test(flavor = "multi_thread")]
async fn constraint_only_change_refreshes_without_restart() {
    let house = temp_house(FIXTURE, "apply-constraint");
    let (mut sup, observer) = up(
        || Supervisor::spawn_at(&house, &[]),
        &["probe", "reflector"],
    )
    .await;
    let probe_pid = running_pid(&observer, "probe").await;
    let reflector_pid = running_pid(&observer, "reflector").await;

    // 7 is inside the fixture's {min = 0, max = 10}.
    config_write(&observer, "home/config/probe/level", json!(7))
        .await
        .expect("write inside the original constraint");

    edit(&house, "units/probe.toml", "max = 10", "max = 5");

    let house_arg = house.to_str().expect("utf-8 path");
    let plan = cli(&["plan", house_arg, "--bus", &sup.endpoint]);
    assert_cli_ok(&plan);
    let text = stdout(&plan);
    assert!(
        text.contains("Plan tier: parameter-only (1 parameter change, 1 manifest refreshed)"),
        "{text}"
    );
    assert!(text.contains("Manifest refreshes (1):"), "{text}");
    assert!(
        text.contains("level: constraint {max=10, min=0} -> {max=5, min=0}"),
        "{text}"
    );

    assert!(
        text.contains(
            "~ probe/level  live=7  repo=1  (replaces a live edit: outside the new constraint)"
        ),
        "{text}"
    );

    let apply = cli(&["apply", house_arg, "--bus", &sup.endpoint]);
    assert_cli_ok(&apply);
    let text = stdout(&apply);
    assert!(text.contains("manifest refreshed: probe"), "{text}");
    assert!(!text.contains("restart"), "no restart steps: {text}");

    // The live store enforces the new constraint immediately...
    let refused = config_write(&observer, "home/config/probe/level", json!(7))
        .await
        .expect_err("7 is above the tightened max");
    assert!(refused.contains("above max 5"), "{refused}");

    // ...the served manifest is the new one, and nothing restarted.
    let manifest = meta_read(&observer, &homeostat::bus::manifest_key("probe"))
        .await
        .expect("meta manifest served");
    assert!(manifest.contains("max = 5"), "{manifest}");
    assert_eq!(
        running_pid(&observer, "probe").await,
        probe_pid,
        "zero restarts"
    );
    assert_eq!(running_pid(&observer, "reflector").await, reflector_pid);

    // The world now matches the repo: the refresh landed durably.
    let replan = cli(&["plan", house_arg, "--bus", &sup.endpoint]);
    assert_cli_ok(&replan);
    assert!(
        stdout(&replan).contains("No changes."),
        "{}",
        stdout(&replan)
    );

    config_write(&observer, "home/config/probe/level", json!(4))
        .await
        .expect("writes inside the new constraint still pass");

    sup.shutdown();
}

/// A live edit survives an apply that leaves its default alone, including
/// one that restarts the unit. `--reset-params` replaces it, and so does a
/// changed default. Plan says which happens before apply runs.
#[tokio::test(flavor = "multi_thread")]
async fn a_live_edit_survives_an_apply_that_keeps_its_default() {
    let house = temp_house(FIXTURE, "apply-keep-edit");
    let (mut sup, observer) = up(
        || Supervisor::spawn_at(&house, &[]),
        &["probe", "reflector"],
    )
    .await;
    let echo_sub = observer
        .declare_subscriber("home/state/den/probe_echo/level")
        .await
        .expect("echo subscriber");
    let probe_pid = running_pid(&observer, "probe").await;
    let house_arg = house.to_str().expect("utf-8 path");

    config_write(&observer, "home/config/probe/level", json!(7))
        .await
        .expect("write inside the constraint");
    // The running probe echoes 7. Drain it, so the echo awaited after the
    // apply comes from the restarted probe.
    await_echo(&echo_sub, json!(7)).await;
    while let Ok(Some(_)) = echo_sub.try_recv() {}

    // An unrelated manifest edit restarts the probe and keeps the edit.
    edit(
        &house,
        "units/probe.toml",
        "Echoes its live level parameter",
        "Echoes the live level parameter",
    );
    let plan = cli(&["plan", house_arg, "--bus", &sup.endpoint]);
    assert_cli_ok(&plan);
    let text = stdout(&plan);
    assert!(text.contains("Live edits kept (1):"), "{text}");
    assert!(text.contains("= probe/level  live=7  repo=1"), "{text}");
    assert!(!text.contains("Parameter changes"), "{text}");

    let apply = cli(&["apply", house_arg, "--bus", &sup.endpoint]);
    assert_cli_ok(&apply);
    let text = stdout(&apply);
    assert!(text.contains("restart probe: ok"), "{text}");
    assert!(!text.contains("parameter probe/level"), "{text}");
    assert_ne!(
        running_pid(&observer, "probe").await,
        probe_pid,
        "restarted"
    );
    assert_eq!(
        cache_read(&observer, "home/config/probe/level").await,
        Some(json!(7)),
        "the core keeps the live edit"
    );
    await_echo(&echo_sub, json!(7)).await;

    let replan = cli(&["plan", house_arg, "--bus", &sup.endpoint]);
    assert_cli_ok(&replan);
    let text = stdout(&replan);
    assert!(
        text.contains("No changes. The live edits above stay."),
        "{text}"
    );

    // --reset-params puts the default back.
    let plan = cli(&["plan", house_arg, "--bus", &sup.endpoint, "--reset-params"]);
    assert_cli_ok(&plan);
    let text = stdout(&plan);
    assert!(
        text.contains("~ probe/level  live=7  repo=1  (replaces a live edit: reset)"),
        "{text}"
    );
    let apply = cli(&["apply", house_arg, "--bus", &sup.endpoint, "--reset-params"]);
    assert_cli_ok(&apply);
    assert!(
        stdout(&apply).contains("parameter probe/level = 1"),
        "{}",
        stdout(&apply)
    );
    assert_eq!(
        cache_read(&observer, "home/config/probe/level").await,
        Some(json!(1))
    );

    // A changed default replaces a live edit.
    config_write(&observer, "home/config/probe/level", json!(7))
        .await
        .expect("write inside the constraint");
    edit(&house, "units/probe.toml", "default = 1", "default = 3");
    let plan = cli(&["plan", house_arg, "--bus", &sup.endpoint]);
    assert_cli_ok(&plan);
    let text = stdout(&plan);
    assert!(
        text.contains("~ probe/level  live=7  repo=3  (replaces a live edit: default changed)"),
        "{text}"
    );
    let apply = cli(&["apply", house_arg, "--bus", &sup.endpoint]);
    assert_cli_ok(&apply);
    assert_eq!(
        cache_read(&observer, "home/config/probe/level").await,
        Some(json!(3))
    );
    await_echo(&echo_sub, json!(3)).await;

    sup.shutdown();
}

/// Live edits survive a supervisor restart. One whose default changed while
/// the supervisor was down is dropped, and the owner's default holds.
#[tokio::test(flavor = "multi_thread")]
async fn a_live_edit_survives_a_supervisor_restart() {
    let house = temp_house(FIXTURE, "restart-keeps-edit");
    let saved = house.join("data/live-edits.json");
    let house_arg = house.to_str().expect("utf-8 path");

    let (mut sup, observer) = up(
        || Supervisor::spawn_at(&house, &[]),
        &["probe", "reflector"],
    )
    .await;
    config_write(&observer, "home/config/probe/level", json!(7))
        .await
        .expect("write inside the constraint");
    assert!(saved.exists(), "the edit is saved");
    drop(observer);
    sup.shutdown();

    let (mut sup, observer) = up(
        || Supervisor::spawn_at(&house, &[]),
        &["probe", "reflector"],
    )
    .await;
    assert_eq!(
        cache_read(&observer, "home/config/probe/level").await,
        Some(json!(7)),
        "the edit is restored"
    );
    let plan = cli(&["plan", house_arg, "--bus", &sup.endpoint]);
    assert_cli_ok(&plan);
    let text = stdout(&plan);
    assert!(text.contains("= probe/level  live=7  repo=1"), "{text}");
    drop(observer);
    sup.shutdown();

    // The owner changes the default while the supervisor is down.
    edit(&house, "units/probe.toml", "default = 1", "default = 3");
    let (mut sup, observer) = up(
        || Supervisor::spawn_at(&house, &[]),
        &["probe", "reflector"],
    )
    .await;
    assert_eq!(
        cache_read(&observer, "home/config/probe/level").await,
        Some(json!(3)),
        "the changed default wins"
    );
    assert!(!saved.exists(), "the dropped edit is gone from the file");

    sup.shutdown();
}

/// Waits up to 10 s for the probe to echo `want`.
async fn await_echo(echo_sub: &StateSub, want: Value) {
    let what = format!("echo of {want}");
    await_sample(echo_sub, Duration::from_secs(10), &what, |msg| {
        msg.json.as_ref() == Some(&want)
    })
    .await;
}

fn add_watcher_pair(house: &Path, adapter: &str, room: &str, entity: &str, automation: &str) {
    std::fs::write(
        house.join(format!("units/{adapter}.toml")),
        format!(
            r#"schema = 1

[unit]
name = "{adapter}"
kind = "adapter"

[runtime]
command = "fake_adapter --crash-after-ms 0"
restart = "on-failure"

[discovery]
mode = "static"
endpoint = "fake://local"

[entities]
dir = "entities/{adapter}/"
"#
        ),
    )
    .expect("write adapter manifest");
    std::fs::create_dir_all(house.join(format!("entities/{adapter}"))).expect("entities dir");
    std::fs::write(
        house.join(format!("entities/{adapter}/{entity}.toml")),
        format!(
            r#"schema = 1

[entity]
id = "{entity}-1"
capability = "light"
room = "{room}"

[write_policy]
mode = "shared"
owner = "{adapter}"
"#
        ),
    )
    .expect("write entity file");
    std::fs::write(
        house.join(format!("units/{automation}.toml")),
        format!(
            r#"schema = 1

[unit]
name = "{automation}"
kind = "automation"

[runtime]
command = "reflector"
restart = "on-failure"

[bus.publishes]
lights = {{ key = "home/cmd/{room}/{entity}/on", capability = "light", priority = "automation" }}
"#
        ),
    )
    .expect("write automation manifest");
}

/// (c) A structural change (a new adapter plus a new automation granted
/// onto its entity) plans as structural with the grant diff rendered, and
/// apply starts units in grant order: the adapter before the dependent
/// automation. Untouched units keep their pids.
#[tokio::test(flavor = "multi_thread")]
async fn structural_change_starts_units_in_grant_order() {
    let house = temp_house(FIXTURE, "apply-structural");
    let (mut sup, observer) = up(
        || Supervisor::spawn_at(&house, &[]),
        &["probe", "reflector"],
    )
    .await;
    let probe_pid = running_pid(&observer, "probe").await;
    let reflector_pid = running_pid(&observer, "reflector").await;

    add_watcher_pair(&house, "beacon", "den", "beacon_lamp", "watcher");
    // The beacon must come up for the walk to proceed.
    edit(
        &house,
        "units/beacon.toml",
        "fake_adapter --crash-after-ms 0",
        "fake_adapter",
    );

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
    assert_eq!(
        running_pid(&observer, "probe").await,
        probe_pid,
        "untouched"
    );
    assert_eq!(
        running_pid(&observer, "reflector").await,
        reflector_pid,
        "untouched"
    );

    sup.shutdown();
}

/// (c2) Moving a granted entity between rooms is structural, not a bare
/// adapter restart: the grant table records each granted entity's room,
/// so the move surfaces as a grant delta the owner reviews.
#[tokio::test(flavor = "multi_thread")]
async fn entity_move_plans_as_structural() {
    let house = temp_house(FIXTURE, "apply-move");
    let (mut sup, _observer) = up(
        || Supervisor::spawn_at(&house, &[]),
        &["probe", "reflector"],
    )
    .await;

    add_watcher_pair(&house, "beacon", "den", "beacon_lamp", "watcher");
    edit(
        &house,
        "units/beacon.toml",
        "fake_adapter --crash-after-ms 0",
        "fake_adapter",
    );
    let house_arg = house.to_str().expect("utf-8 path");
    let apply = cli(&["apply", house_arg, "--bus", &sup.endpoint]);
    assert_cli_ok(&apply);

    edit(
        &house,
        "entities/beacon/beacon_lamp.toml",
        "room = \"den\"",
        "room = \"attic\"",
    );

    let plan = cli(&["plan", house_arg, "--bus", &sup.endpoint]);
    assert_cli_ok(&plan);
    let text = stdout(&plan);
    assert!(text.contains("Plan tier: structural"), "{text}");
    assert!(
        text.contains("Grant changes:"),
        "the move renders as a grant delta: {text}"
    );

    sup.shutdown();
}

/// (c3) Moving an entity nobody is granted onto is structural too: the
/// owner's state row records every bound entity, so the move is a grant
/// delta rendered with the entity's old and new facts rather than a bare
/// adapter restart on a `files_hash` change (docs/design.md#tiers: entity
/// moves are structural).
#[tokio::test(flavor = "multi_thread")]
async fn ungranted_entity_move_plans_as_structural() {
    let house = temp_house(FIXTURE, "apply-move-ungranted");
    let (mut sup, _observer) = up(
        || Supervisor::spawn_at(&house, &[]),
        &["probe", "reflector"],
    )
    .await;

    edit(
        &house,
        "entities/reflector/lamp.toml",
        "room = \"livingroom\"",
        "room = \"attic\"",
    );

    let house_arg = house.to_str().expect("utf-8 path");
    let plan = cli(&["plan", house_arg, "--bus", &sup.endpoint]);
    assert_cli_ok(&plan);
    let text = stdout(&plan);
    assert!(text.contains("Plan tier: structural"), "{text}");
    assert!(text.contains("Grant changes:"), "{text}");
    assert!(text.contains("+ reflector.state  binds"), "{text}");
    assert!(
        text.contains("-> lamp  (room=attic, capability=light, write=shared, owner=reflector)"),
        "{text}"
    );
    assert!(text.contains("- reflector.state  binds"), "{text}");
    assert!(
        text.contains(
            "-> lamp  (room=livingroom, capability=light, write=shared, owner=reflector)"
        ),
        "{text}"
    );

    sup.shutdown();
}

/// (d) A unit that fails to become ready mid-walk halts the apply in
/// place and reports position: earlier units keep running, later units
/// are not started, and a re-plan shows only the remaining work.
#[tokio::test(flavor = "multi_thread")]
async fn failing_unit_halts_walk_in_place() {
    let house = temp_house(FIXTURE, "apply-halt");
    let (mut sup, observer) = up(
        || Supervisor::spawn_at(&house, &[]),
        &["probe", "reflector"],
    )
    .await;
    let probe_pid = running_pid(&observer, "probe").await;
    let reflector_pid = running_pid(&observer, "reflector").await;

    // The broken adapter crashes on spawn until its breaker opens; the
    // waiter automation is granted onto its entity, so it walks second.
    add_watcher_pair(&house, "broken", "attic", "broken_light", "waiter");

    let house_arg = house.to_str().expect("utf-8 path");
    let apply = cli(&["apply", house_arg, "--bus", &sup.endpoint]);
    assert!(!apply.status.success(), "apply reports failure");
    let out = stdout(&apply);
    let err = stderr(&apply);
    assert!(
        out.contains("start broken: FAILED (circuit breaker open)"),
        "{out}"
    );
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
    // The halted walk's rows stay pending, since a unit it restarted runs
    // on its table, and the arbiter routes on them until an apply completes.
    let pending: serde_json::Value = serde_json::from_str(
        &meta_read(&observer, homeostat::bus::GRANTS_PENDING_KEY)
            .await
            .expect("pending grants served"),
    )
    .expect("pending grants are JSON");
    assert!(
        pending
            .as_array()
            .is_some_and(|rows| rows.iter().any(|g| g["unit"] == json!("waiter"))),
        "{pending}"
    );

    // A re-plan shows only the remaining work.
    let plan = cli(&["plan", house_arg, "--bus", &sup.endpoint]);
    assert_cli_ok(&plan);
    let text = stdout(&plan);
    assert!(
        text.contains("Plan tier: structural (2 units created, 1 grant added)"),
        "the halted walk left the remaining work planned: {text}"
    );

    sup.shutdown();
}

/// (e) A pending plan whose base commit is stale refuses to apply.
#[tokio::test(flavor = "multi_thread")]
async fn stale_pending_plan_refuses_to_apply() {
    let house = temp_house(FIXTURE, "apply-stale");
    git_init_commit(&house);
    let (mut sup, observer) = up(
        || Supervisor::spawn_at(&house, &[]),
        &["probe", "reflector"],
    )
    .await;

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

    let apply = cli(&[
        "apply",
        house_arg,
        "--bus",
        &sup.endpoint,
        "--plan",
        &plan_path,
    ]);
    assert!(!apply.status.success(), "stale plan must refuse");
    let err = stderr(&apply);
    assert!(err.contains("stale"), "{err}");

    // Nothing was applied: the live value is still the old default.
    assert_eq!(
        cache_read(&observer, "home/config/probe/level").await,
        Some(json!(1))
    );

    sup.shutdown();
}

/// A pending plan saved with `--reset-params` applies the reset it showed.
/// The plan file carries the setting, and `apply --plan` refuses the flag.
#[tokio::test(flavor = "multi_thread")]
async fn a_pending_reset_plan_applies_its_reset() {
    let house = temp_house(FIXTURE, "apply-pending-reset");
    git_init_commit(&house);
    let (mut sup, observer) = up(
        || Supervisor::spawn_at(&house, &[]),
        &["probe", "reflector"],
    )
    .await;

    config_write(&observer, "home/config/probe/level", json!(7))
        .await
        .expect("write inside the constraint");

    let house_arg = house.to_str().expect("utf-8 path");
    let saved = cli(&[
        "plan",
        house_arg,
        "--bus",
        &sup.endpoint,
        "--save",
        "--reset-params",
    ]);
    assert_cli_ok(&saved);
    let text = stdout(&saved);
    assert!(
        text.contains("~ probe/level  live=7  repo=1  (replaces a live edit: reset)"),
        "{text}"
    );
    let plan_path = text
        .lines()
        .find_map(|l| l.strip_prefix("Pending plan saved: "))
        .expect("saved path printed")
        .to_string();

    let both = cli(&[
        "apply",
        house_arg,
        "--bus",
        &sup.endpoint,
        "--plan",
        &plan_path,
        "--reset-params",
    ]);
    assert!(!both.status.success(), "--plan and --reset-params conflict");

    let apply = cli(&[
        "apply",
        house_arg,
        "--bus",
        &sup.endpoint,
        "--plan",
        &plan_path,
    ]);
    assert_cli_ok(&apply);
    assert!(
        stdout(&apply).contains("parameter probe/level = 1"),
        "{}",
        stdout(&apply)
    );
    assert_eq!(
        cache_read(&observer, "home/config/probe/level").await,
        Some(json!(1))
    );

    sup.shutdown();
}
