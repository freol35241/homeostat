//! The apply engine (docs/design.md#the-apply-walk). The CLI commands the
//! running supervisor through a control queryable at `home/meta/system/apply`,
//! where a GET with payload is an apply request. The supervisor re-reads the
//! repo, derives its own diff against its in-memory world, and executes the
//! walk one unit at a time. Parameters come first and restart nothing. Then
//! removals run in reverse grant order, and creates and restarts in grant
//! order, each waiting for health `running`. A failure halts the walk in
//! place.
//!
//! The supervisor holds the apply lock, so only one apply runs at a time.
//! Parameter-only applies skip the lock. An apply restart spawns a fresh
//! supervision task, so the unit gets a fresh breaker. New code starts with a
//! fresh failure budget, and a unit stuck in backoff or with its breaker open
//! can be replaced mid-cycle.

use std::sync::Arc;

use crate::bus::{self, ApplyParam, ApplyRequest, ApplyResult, ApplyStep};
use crate::plan::{self, StepAction, Tier};
use crate::supervisor::unit::UnitSpec;
use crate::supervisor::Core;

/// Declares the apply queryable and serves each request in its own task.
pub async fn serve(core: Arc<Core>) -> Result<(), String> {
    let queryable = core
        .session
        .declare_queryable(bus::APPLY_KEY)
        .await
        .map_err(|e| format!("failed to declare apply queryable: {e}"))?;
    tokio::spawn(async move {
        while let Ok(query) = queryable.recv_async().await {
            // Each request runs in its own task so a parameter-only apply does
            // not queue behind a structural walk.
            let core = core.clone();
            tokio::spawn(async move { handle(core, query).await });
        }
    });
    Ok(())
}

async fn handle(core: Arc<Core>, query: zenoh::query::Query) {
    let Some(payload) = query.payload() else {
        // A GET without payload is not an apply; the meta queryable owns reads.
        return;
    };
    let request: ApplyRequest = match serde_json::from_slice(&payload.to_bytes()) {
        Ok(request) => request,
        Err(err) => {
            let _ = query
                .reply_err(format!("apply request is not valid JSON: {err}"))
                .await;
            return;
        }
    };
    let result = execute(&core, request).await;
    let payload = serde_json::to_string(&result).expect("apply result serializes");
    if result.ok {
        let _ = query.reply(bus::APPLY_KEY, payload).await;
    } else {
        let _ = query.reply_err(payload).await;
    }
}

fn failure(error: String) -> ApplyResult {
    ApplyResult {
        ok: false,
        params: Vec::new(),
        refreshes: Vec::new(),
        steps: Vec::new(),
        halted_at: None,
        not_reached: Vec::new(),
        error: Some(error),
    }
}

async fn execute(core: &Arc<Core>, request: ApplyRequest) -> ApplyResult {
    let check = crate::check(&core.root);
    if !check.errors.is_empty() {
        return failure(format!(
            "repo failed validation:\n{}",
            crate::error::render_sorted(&check.errors).join("\n")
        ));
    }

    let world = core.snapshot();
    let diff = plan::diff(&check, &core.root, &world);
    if diff.is_empty() {
        return ApplyResult {
            ok: true,
            params: Vec::new(),
            refreshes: Vec::new(),
            steps: Vec::new(),
            halted_at: None,
            not_reached: Vec::new(),
            error: None,
        };
    }
    let tier = plan::derive_tier(&diff);

    // One apply at a time; the parameter fast path is exempt.
    let _guard = if tier == Tier::ParameterOnly {
        None
    } else {
        match core.apply_lock.try_lock() {
            Ok(guard) => Some(guard),
            Err(_) => return failure("an apply is already in progress".to_string()),
        }
    };

    // Parameters first. The repo is the system of record, so live values reset
    // to repo defaults. Every subscribed unit sees the put, and nothing
    // restarts. A unit restarted later in the walk reads its values with a get
    // anyway.
    let mut params = Vec::new();
    {
        // Swap and puts happen together, in order. Otherwise a config write
        // racing this block could validate against the new store and then have
        // its bus put overwritten by the older repo value below.
        let _write_guard = core.store.write_lock().await;
        for (unit, param, value) in core.store.replace_from_house(&check.house) {
            let _ = core
                .session
                .put(bus::config_key(&unit, &param), value.to_string())
                .await;
            params.push(ApplyParam { unit, param, value });
        }
    }

    // Parameter-level manifest changes (default/constraint/editable_by). The
    // store rebuild above already enforces the new spec. Recording the unit
    // refreshes the served meta, so later plans and manifest readers see the
    // new manifest without a restart.
    let mut refreshes = Vec::new();
    for refresh in &diff.refreshes {
        let loaded = check
            .house
            .unit(&refresh.name)
            .expect("refresh of a repo unit");
        core.record_unit(
            &refresh.name,
            plan::world_unit_from_repo(&core.root, loaded, &check.house, &check.expanded),
        )
        .await;
        refreshes.push(refresh.name.clone());
    }

    let walk = plan::walk_steps(&diff, &check, &world);
    let mut steps: Vec<ApplyStep> = Vec::new();
    let mut halted_at = None;
    let mut not_reached = Vec::new();

    for (index, step) in walk.iter().enumerate() {
        if core.shutting_down() {
            steps.push(ApplyStep {
                unit: step.unit.clone(),
                action: step.action.to_string(),
                ok: false,
                error: Some("supervisor shutting down".to_string()),
            });
            halted_at = Some(step.unit.clone());
            not_reached = walk[index + 1..].iter().map(|s| s.unit.clone()).collect();
            break;
        }
        let outcome: Result<(), String> = match step.action {
            StepAction::Stop => {
                core.destroy(&step.unit).await;
                Ok(())
            }
            StepAction::Start | StepAction::Restart => {
                core.stop(&step.unit).await;
                let loaded = check
                    .house
                    .unit(&step.unit)
                    .expect("walk step for a repo unit");
                core.launch(UnitSpec::from_loaded(loaded, &core.root, &core.listen))
                    .await;
                match core.await_ready(&step.unit).await {
                    Ok(()) => {
                        core.record_unit(
                            &step.unit,
                            plan::world_unit_from_repo(
                                &core.root,
                                loaded,
                                &check.house,
                                &check.expanded,
                            ),
                        )
                        .await;
                        Ok(())
                    }
                    Err(reason) => Err(reason),
                }
            }
        };
        let ok = outcome.is_ok();
        steps.push(ApplyStep {
            unit: step.unit.clone(),
            action: step.action.to_string(),
            ok,
            error: outcome.err(),
        });
        if !ok {
            halted_at = Some(step.unit.clone());
            not_reached = walk[index + 1..].iter().map(|s| s.unit.clone()).collect();
            break;
        }
    }

    let ok = halted_at.is_none();
    if ok {
        // The grant table and the applied commit advance only when the whole
        // plan applied, so a halted walk re-plans the remaining work.
        core.record_grants(check.grants.clone()).await;
        if let Some(commit) = request.base_commit {
            core.record_applied_commit(commit).await;
        }
    }
    ApplyResult {
        ok,
        params,
        refreshes,
        steps,
        halted_at,
        not_reached,
        error: None,
    }
}
