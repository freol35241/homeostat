//! The apply engine (docs/design.md, step 5b): the CLI commands the running
//! supervisor through a control queryable at `home/meta/system/apply` — a
//! GET with payload is an apply request. The supervisor re-reads the repo,
//! derives its own diff against its in-memory world, and executes the walk
//! per-unit and rolling: parameters first (no restarts), removals in
//! reverse grant order, creates/restarts in grant order, awaiting health
//! `running` after each. Failure halts the walk in place.
//!
//! The supervisor holds the apply lock — one apply at a time. Parameter-only
//! applies bypass it (the fast path). A deliberate apply restart spawns a
//! fresh supervision task, so the unit gets a fresh breaker: new code earns
//! a fresh failure budget, and a unit stuck in backoff/open can be replaced
//! mid-cycle.

use std::sync::Arc;

use crate::bus::{self, ApplyParam, ApplyRequest, ApplyResult, ApplyStep};
use crate::plan::{self, StepAction, Tier};
use crate::supervisor::unit::UnitSpec;
use crate::supervisor::Core;

pub async fn serve(core: Arc<Core>) -> Result<(), String> {
    let queryable = core
        .session
        .declare_queryable(bus::APPLY_KEY)
        .await
        .map_err(|e| format!("failed to declare apply queryable: {e}"))?;
    tokio::spawn(async move {
        while let Ok(query) = queryable.recv_async().await {
            // Each request runs in its own task so a parameter-only apply
            // is never queued behind a structural walk.
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
        tier: None,
        params: Vec::new(),
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
            tier: None,
            params: Vec::new(),
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

    // Parameters first: the repo is the system of record, live values
    // reset to repo defaults, every subscribed unit sees the put — no
    // restart. A unit restarted later in the walk seeds via get anyway.
    let mut params = Vec::new();
    for (unit, param, value) in core.store.replace_from_house(&check.house) {
        let _ = core
            .session
            .put(bus::config_key(&unit, &param), value.to_string())
            .await;
        params.push(ApplyParam { unit, param, value });
    }

    let walk = plan::walk_steps(&diff, &check, &world);
    let mut steps: Vec<ApplyStep> = Vec::new();
    let mut halted_at = None;
    let mut not_reached = Vec::new();

    for (index, step) in walk.iter().enumerate() {
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
        // The grant table and the applied commit advance only on a fully
        // applied plan: a halted walk re-plans exactly the remaining work.
        core.record_grants(check.grants.clone()).await;
        if let Some(commit) = request.base_commit {
            core.record_applied_commit(commit).await;
        }
    }
    ApplyResult {
        ok,
        tier: Some(tier.to_string()),
        params,
        steps,
        halted_at,
        not_reached,
        error: None,
    }
}
