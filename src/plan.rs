//! The plan engine: diffs a validated house repo against the world as the bus
//! reports it (see docs/design.md#plan-and-apply). There is no state file.
//! Desired state is the repo and actual state is queried from the bus, so
//! there is no stored copy that can drift.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::Path;

use serde_json::Value;

use crate::content;
use crate::expand::ExpandedKey;
use crate::grants::{free_nodes, grant_edges, Grant};
use crate::manifest::{implicit_enabled, ParamSpec, UnitKind, ENABLED_PARAM};
use crate::params::{default_value, display_value, keeps_live};
use crate::repo::LoadedUnit;
use crate::wiring::Contributor;
use crate::CheckResult;

/// Plan tiers per docs/design.md#tiers. They are derived from the diff, and
/// nothing declares them. Any grant-table delta escalates to structural.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    ParameterOnly,
    Behavioral,
    Structural,
}

impl fmt::Display for Tier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Tier::ParameterOnly => "parameter-only",
            Tier::Behavioral => "behavioral",
            Tier::Structural => "structural",
        })
    }
}

/// One unit as the world reports it: what is running.
#[derive(Debug, Clone)]
pub struct WorldUnit {
    /// The manifest TOML as the supervisor loaded it.
    pub manifest: Vec<u8>,
    pub manifest_hash: String,
    pub files_hash: String,
}

impl WorldUnit {
    /// The kind the unit's manifest declares. `None` when the manifest does
    /// not parse or its `[unit].kind` is missing or not a known kind.
    fn kind(&self) -> Option<UnitKind> {
        let text = std::str::from_utf8(&self.manifest).ok()?;
        let manifest: toml::Value = toml::from_str(text).ok()?;
        manifest.get("unit")?.get("kind")?.clone().try_into().ok()
    }
}

/// The actual world, read from the bus (manifest hashes, current parameters,
/// resolved grants), or empty when planning offline.
#[derive(Debug, Default)]
pub struct World {
    /// Label for plan output: "empty" or the bus endpoint.
    pub label: String,
    /// True when this world was read from a live bus.
    pub live: bool,
    pub units: BTreeMap<String, WorldUnit>,
    pub params: BTreeMap<(String, String), Value>,
    pub grants: Vec<Grant>,
    pub applied_commit: Option<String>,
}

impl World {
    /// The world before anything was applied: no units, no grants. `plan`
    /// without a bus diffs against it.
    pub fn empty() -> World {
        World {
            label: "empty".to_string(),
            ..World::default()
        }
    }
}

/// The unit's world entry as the repo currently describes it. The supervisor
/// records this at startup and after a successful apply step, and [`diff`]
/// compares it with the world's entry.
pub fn world_unit_from_repo(
    root: &Path,
    unit: &LoadedUnit,
    house: &crate::repo::House,
    expanded: &[ExpandedKey],
) -> WorldUnit {
    let name = &unit.manifest.unit.name;
    let manifest = fs::read(root.join(&unit.path)).unwrap_or_default();
    WorldUnit {
        manifest_hash: content::manifest_hash(&manifest),
        files_hash: content::files_hash(root, unit, house, content::uses_zone(name, expanded)),
        manifest,
    }
}

/// A unit the apply restarts, and why.
#[derive(Debug)]
pub struct Restart {
    pub name: String,
    pub reason: String,
    /// The manifest itself changed (not only unit files): the unit's key
    /// surface may have changed, so the plan renders its expansion.
    pub manifest_changed: bool,
}

/// A parameter whose live value differs from its repo default.
#[derive(Debug)]
pub struct ParamChange {
    pub unit: String,
    pub param: String,
    pub live: Value,
    pub repo: Value,
    /// Why the apply replaces a live edit, when the live value is one.
    /// `None` when the live value is the old default.
    pub replaces_edit: Option<&'static str>,
}

/// A unit whose manifest changed at parameter level only (the fields
/// `param_level_only` strips). It needs no restart, but the running world must
/// refresh: the config store enforces the new constraints, and the served meta
/// manifest updates so plans and the dashboard see it.
#[derive(Debug)]
pub struct Refresh {
    pub name: String,
    /// Per-param deltas, rendered "param: field old -> new".
    pub changes: Vec<String>,
}

/// Everything an apply would change: the repo against the world.
#[derive(Debug, Default)]
pub struct Diff {
    pub creates: Vec<String>,
    pub destroys: Vec<String>,
    pub restarts: Vec<Restart>,
    pub params: Vec<ParamChange>,
    /// Live edits the apply keeps ([`keeps_live`]). They are not changes.
    pub kept: Vec<ParamChange>,
    pub refreshes: Vec<Refresh>,
    pub grant_adds: Vec<Grant>,
    pub grant_removes: Vec<Grant>,
}

impl Diff {
    /// Whether the world already matches the repo.
    pub fn is_empty(&self) -> bool {
        self.creates.is_empty()
            && self.destroys.is_empty()
            && self.restarts.is_empty()
            && self.params.is_empty()
            && self.refreshes.is_empty()
            && self.grant_adds.is_empty()
            && self.grant_removes.is_empty()
    }
}

/// The plan's tier: structural with any create, destroy or grant change,
/// behavioral with any restart, parameter-only otherwise.
pub fn derive_tier(diff: &Diff) -> Tier {
    if !diff.creates.is_empty()
        || !diff.destroys.is_empty()
        || !diff.grant_adds.is_empty()
        || !diff.grant_removes.is_empty()
    {
        Tier::Structural
    } else if !diff.restarts.is_empty() {
        Tier::Behavioral
    } else {
        Tier::ParameterOnly
    }
}

/// Diffs the validated repo against the world. Only call on an error-free
/// check result. `reset_params` plans every live value back to its default.
pub fn diff(check: &CheckResult, root: &Path, world: &World, reset_params: bool) -> Diff {
    let mut diff = Diff::default();
    let repo_names: BTreeSet<&str> = check
        .house
        .units
        .iter()
        .map(|u| u.manifest.unit.name.as_str())
        .collect();

    for unit in &check.house.units {
        let name = &unit.manifest.unit.name;
        let Some(world_unit) = world.units.get(name) else {
            diff.creates.push(name.clone());
            continue;
        };

        let repo_unit = world_unit_from_repo(root, unit, &check.house, &check.expanded);
        let files_changed = repo_unit.files_hash != world_unit.files_hash;
        let hash_changed = repo_unit.manifest_hash != world_unit.manifest_hash;
        let repo_manifest = parse_manifest(&repo_unit.manifest);
        let world_manifest = parse_manifest(&world_unit.manifest);
        let param_level =
            hash_changed && param_level_only(repo_manifest.as_ref(), world_manifest.as_ref());
        let manifest_changed = hash_changed && !param_level;
        if manifest_changed || files_changed {
            let reason = match (manifest_changed, files_changed) {
                (true, true) => "manifest and unit files changed",
                (true, false) => "manifest changed",
                _ => "unit files changed",
            };
            diff.restarts.push(Restart {
                name: name.clone(),
                reason: reason.to_string(),
                manifest_changed,
            });
        } else if param_level {
            diff.refreshes.push(Refresh {
                name: name.clone(),
                changes: refresh_changes(repo_manifest.as_ref(), world_manifest.as_ref()),
            });
        }

        // Parameter diffs: live value against repo default, with the
        // world's manifest giving the default the live value was set under.
        if let Some(params) = &unit.manifest.params {
            for (param, spec) in params {
                let repo = default_value(spec);
                let Some(live) = world.params.get(&(name.clone(), param.clone())) else {
                    continue; // unknown to the running world: covered by restart
                };
                if live == &repo {
                    continue;
                }
                let old_default = world_default(world_manifest.as_ref(), param);
                let edited = old_default.as_ref() != Some(live);
                let keep = edited
                    && !reset_params
                    && old_default
                        .as_ref()
                        .is_some_and(|old| keeps_live(old, spec, live));
                let replaces_edit = if keep || !edited {
                    None
                } else if reset_params {
                    Some("reset")
                } else if old_default.as_ref() != Some(&repo) {
                    Some("default changed")
                } else {
                    Some("outside the new constraint")
                };
                let change = ParamChange {
                    unit: name.clone(),
                    param: param.clone(),
                    live: live.clone(),
                    repo,
                    replaces_edit,
                };
                if keep {
                    diff.kept.push(change);
                } else {
                    diff.params.push(change);
                }
            }
        }
    }

    for name in world.units.keys() {
        if !repo_names.contains(name.as_str()) {
            diff.destroys.push(name.clone());
        }
    }

    for grant in &check.grants {
        if !world.grants.contains(grant) {
            diff.grant_adds.push(grant.clone());
        }
    }
    for grant in &world.grants {
        if !check.grants.contains(grant) {
            diff.grant_removes.push(grant.clone());
        }
    }

    diff.creates.sort();
    diff.destroys.sort();
    diff.restarts.sort_by(|a, b| a.name.cmp(&b.name));
    diff
}

/// A manifest's bytes as TOML. `None` when they do not parse.
fn parse_manifest(bytes: &[u8]) -> Option<toml::Value> {
    toml::from_str(std::str::from_utf8(bytes).ok()?).ok()
}

/// A parameter's default in the manifest the world is running, the implicit
/// `enabled` of an automation included. `None` when the manifest does not
/// parse or does not have the parameter.
fn world_default(manifest: Option<&toml::Value>, param: &str) -> Option<Value> {
    let manifest = manifest?;
    let declared = manifest.get("params").and_then(|p| p.get(param));
    let spec: ParamSpec = match declared {
        Some(spec) => spec.clone().try_into().ok()?,
        None if param == ENABLED_PARAM
            && manifest.get("unit")?.get("kind")?.as_str()? == "automation" =>
        {
            implicit_enabled()
        }
        None => return None,
    };
    Some(default_value(&spec))
}

/// True when the two manifests differ only in parameter values: with every
/// param's `default`, `constraint` and `editable_by` stripped, the rest is
/// equal. Adding or removing a param, or changing its type, is not
/// parameter-level, because the running unit read its manifest at startup. An
/// unparseable side counts as a change.
fn param_level_only(repo: Option<&toml::Value>, world: Option<&toml::Value>) -> bool {
    let (Some(mut a), Some(mut b)) = (repo.cloned(), world.cloned()) else {
        return false;
    };
    strip_param_values(&mut a);
    strip_param_values(&mut b);
    a == b
}

fn strip_param_values(manifest: &mut toml::Value) {
    let Some(params) = manifest.get_mut("params").and_then(|p| p.as_table_mut()) else {
        return;
    };
    for (_, spec) in params.iter_mut() {
        if let Some(table) = spec.as_table_mut() {
            table.remove("default");
            table.remove("constraint");
            table.remove("editable_by");
        }
    }
}

/// The per-param deltas behind a parameter-level manifest change: the
/// stripped fields compared between the world's manifest and the repo's.
/// Empty when the manifests differ only in formatting or comments.
fn refresh_changes(repo: Option<&toml::Value>, world: Option<&toml::Value>) -> Vec<String> {
    let params = |manifest: Option<&toml::Value>| -> toml::value::Table {
        manifest
            .and_then(|m| m.get("params").and_then(|p| p.as_table()).cloned())
            .unwrap_or_default()
    };
    let old = params(world);
    let mut changes = Vec::new();
    for (param, spec) in params(repo) {
        for field in ["default", "constraint", "editable_by"] {
            let new_value = spec.get(field);
            let old_value = old.get(&param).and_then(|s| s.get(field));
            if new_value != old_value {
                changes.push(format!(
                    "{param}: {field} {} -> {}",
                    policy_value(old_value),
                    policy_value(new_value)
                ));
            }
        }
    }
    changes
}

fn policy_value(value: Option<&toml::Value>) -> String {
    match value {
        None => "(none)".to_string(),
        Some(toml::Value::Table(table)) => {
            let parts: Vec<String> = table
                .iter()
                .map(|(k, v)| format!("{k}={}", display_value(v)))
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
        Some(other) => display_value(other),
    }
}

/// The apply walk over units, ordered by the grant table: adapters before the
/// automations granted onto their entities. The order is derived, not
/// declared. Parameter writes are not steps. They happen first and restart
/// nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepAction {
    Stop,
    Start,
    Restart,
}

impl fmt::Display for StepAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            StepAction::Stop => "stop",
            StepAction::Start => "start",
            StepAction::Restart => "restart",
        })
    }
}

/// One unit step of the apply walk.
#[derive(Debug, Clone)]
pub struct Step {
    pub unit: String,
    pub action: StepAction,
}

/// Orders the walk: removals first in reverse grant order (dependents stop
/// before the adapters they write through), then creates and restarts in
/// grant order. Grant-edgeless units and ties order by kind (adapter,
/// automation, service), then name.
pub fn walk_steps(diff: &Diff, check: &CheckResult, world: &World) -> Vec<Step> {
    let mut steps = Vec::new();

    let stop_set: BTreeSet<String> = diff.destroys.iter().cloned().collect();
    let old_edges = grant_edges(&world.grants);
    let mut stops = ordered(&stop_set, &old_edges, |name| {
        world
            .units
            .get(name)
            .and_then(WorldUnit::kind)
            .map_or(3, kind_order)
    });
    stops.reverse();
    steps.extend(stops.into_iter().map(|unit| Step {
        unit,
        action: StepAction::Stop,
    }));

    let mut start_set: BTreeSet<String> = diff.creates.iter().cloned().collect();
    start_set.extend(diff.restarts.iter().map(|r| r.name.clone()));
    let new_edges = grant_edges(&check.grants);
    let creates: BTreeSet<&String> = diff.creates.iter().collect();
    for unit in ordered(&start_set, &new_edges, |name| {
        check
            .house
            .unit(name)
            .map_or(3, |u| kind_order(u.manifest.unit.kind))
    }) {
        let action = if creates.contains(&unit) {
            StepAction::Start
        } else {
            StepAction::Restart
        };
        steps.push(Step { unit, action });
    }
    steps
}

/// Topological order over `set` under `edges` (owner before dependent), each
/// layer sorted by (kind, name). `kind_of` runs once per unit, because a
/// world unit's kind comes from parsing its manifest. Check time refuses a
/// cyclic grant table (`grant-cycle`), so one cannot reach a plan. A
/// malformed table still falls back to sorted order instead of looping.
fn ordered<F>(set: &BTreeSet<String>, edges: &[(String, String)], kind_of: F) -> Vec<String>
where
    F: Fn(&str) -> u8,
{
    let mut remaining: BTreeSet<String> = set.clone();
    let mut out = Vec::new();
    while !remaining.is_empty() {
        let mut layer = free_nodes(&remaining, edges);
        if layer.is_empty() {
            layer = remaining.iter().cloned().collect();
        }
        layer.sort_by_cached_key(|name| (kind_of(name), name.clone()));
        for unit in layer {
            remaining.remove(&unit);
            out.push(unit);
        }
    }
    out
}

fn kind_order(kind: UnitKind) -> u8 {
    match kind {
        UnitKind::Adapter => 0,
        UnitKind::Automation => 1,
        UnitKind::Service => 2,
    }
}

/// Renders the full plan text for `diff`, the result of [`diff`] over the
/// same check and world. Only call on an error-free check result.
pub fn render(check: &CheckResult, diff: &Diff, repo_label: &str, world: &World) -> String {
    let mut out = String::new();
    let mut units: Vec<&LoadedUnit> = check.house.units.iter().collect();
    units.sort_by_key(|u| {
        (
            kind_order(u.manifest.unit.kind),
            u.manifest.unit.name.clone(),
        )
    });

    out.push_str("Homeostat plan\n");
    out.push_str(&format!("  repo:  {repo_label}\n"));
    if world.live {
        let mut label = format!("  world: {} ({} unit", world.label, world.units.len());
        if world.units.len() != 1 {
            label.push('s');
        }
        if let Some(commit) = &world.applied_commit {
            label.push_str(&format!(", applied commit {commit}"));
        }
        label.push_str(")\n");
        out.push_str(&label);
    } else {
        out.push_str(&format!("  world: {}\n", world.label));
    }
    out.push('\n');

    let created: Vec<&LoadedUnit> = units
        .iter()
        .filter(|u| diff.creates.contains(&u.manifest.unit.name))
        .copied()
        .collect();
    if !created.is_empty() {
        out.push_str(&format!("Units to create ({}):\n\n", created.len()));
        for unit in &created {
            render_unit(check, unit, &mut out);
        }
    }

    if !diff.destroys.is_empty() {
        out.push_str(&format!(
            "\nUnits to destroy ({}):\n\n",
            diff.destroys.len()
        ));
        for name in &diff.destroys {
            let kind = world
                .units
                .get(name)
                .and_then(WorldUnit::kind)
                .map_or("unit".to_string(), |kind| kind.to_string());
            out.push_str(&format!("- {kind} {name}\n"));
        }
    }

    if !diff.restarts.is_empty() {
        out.push_str(&format!(
            "\nUnits to restart ({}):\n\n",
            diff.restarts.len()
        ));
        for restart in &diff.restarts {
            let unit = check
                .house
                .unit(&restart.name)
                .expect("restart of a repo unit");
            out.push_str(&format!(
                "~ {} {} ({})\n    reason: {}\n",
                unit.manifest.unit.kind, restart.name, unit.path, restart.reason
            ));
        }
    }

    if !diff.params.is_empty() {
        out.push_str(&format!("\nParameter changes ({}):\n\n", diff.params.len()));
        for change in &diff.params {
            out.push_str(&format!(
                "  ~ {}/{}  live={}  repo={}",
                change.unit, change.param, change.live, change.repo
            ));
            if let Some(reason) = change.replaces_edit {
                out.push_str(&format!("  (replaces a live edit: {reason})"));
            }
            out.push('\n');
        }
    }

    if !diff.kept.is_empty() {
        out.push_str(&format!("\nLive edits kept ({}):\n\n", diff.kept.len()));
        for change in &diff.kept {
            out.push_str(&format!(
                "  = {}/{}  live={}  repo={}\n",
                change.unit, change.param, change.live, change.repo
            ));
        }
    }

    if !diff.refreshes.is_empty() {
        out.push_str(&format!(
            "\nManifest refreshes ({}):\n\n",
            diff.refreshes.len()
        ));
        for refresh in &diff.refreshes {
            let unit = check
                .house
                .unit(&refresh.name)
                .expect("refresh of a repo unit");
            out.push_str(&format!("  ~ {} ({})\n", refresh.name, unit.path));
            for change in &refresh.changes {
                out.push_str(&format!("      {change}\n"));
            }
        }
    }

    // Created units and units restarting on a manifest change may both bring
    // new key surface, and an approval prompt must show it.
    let mut with_keys = created.clone();
    with_keys.extend(
        diff.restarts
            .iter()
            .filter(|r| r.manifest_changed)
            .filter_map(|r| check.house.unit(&r.name)),
    );
    if !with_keys.is_empty() {
        out.push_str("\nExpanded keys:\n\n");
        for unit in &with_keys {
            let name = &unit.manifest.unit.name;
            for key in check.expanded.iter().filter(|k| &k.unit == name) {
                render_expanded(key, &mut out);
            }
        }
    }

    if world.live {
        if !diff.grant_adds.is_empty() || !diff.grant_removes.is_empty() {
            out.push_str("\nGrant changes:\n\n");
            for grant in &diff.grant_adds {
                render_grant(grant, '+', &mut out);
            }
            for grant in &diff.grant_removes {
                render_grant(grant, '-', &mut out);
            }
        }
    } else {
        out.push_str("\nGrant table:\n\n");
        if check.grants.is_empty() {
            out.push_str("  (none)\n");
        }
        for grant in &check.grants {
            render_grant(grant, ' ', &mut out);
        }
    }

    // Device feeds, printed only when the house wires any, so a house without
    // them gets no empty section.
    if !check.feeds.is_empty() {
        out.push_str("\nFeeds:\n\n");
        for feed in &check.feeds {
            render_contributor(&feed.entity, &feed.input, &feed.source, &mut out);
        }
    }

    // Declared sources: what a computed value is derived from. Printed on the
    // same terms as feeds, only when a house declares any.
    if !check.sources.is_empty() {
        out.push_str("\nSources:\n\n");
        for source in &check.sources {
            render_contributor(&source.entity, &source.name, &source.contributor, &mut out);
            if let Some(note) = &source.note {
                out.push_str(&format!("      {note}\n"));
            }
        }
    }

    if !check.warnings.is_empty() {
        out.push_str("\nWarnings:\n\n");
        for warning in &check.warnings {
            out.push_str(&format!("  {warning}\n"));
        }
    }

    if diff.is_empty() && diff.kept.is_empty() {
        out.push_str("\nNo changes. The world matches the repo.\n");
    } else if diff.is_empty() {
        out.push_str("\nNo changes. The live edits above stay.\n");
    } else {
        out.push_str(&format!(
            "\nPlan tier: {} ({})\n",
            derive_tier(diff),
            summarize(diff)
        ));
    }

    out
}

fn summarize(diff: &Diff) -> String {
    let mut parts = Vec::new();
    let count = |n: usize, singular: &str, plural: &str, verb: &str| {
        format!("{n} {} {verb}", if n == 1 { singular } else { plural })
    };
    if !diff.creates.is_empty() {
        parts.push(count(diff.creates.len(), "unit", "units", "created"));
    }
    if !diff.destroys.is_empty() {
        parts.push(count(diff.destroys.len(), "unit", "units", "destroyed"));
    }
    if !diff.restarts.is_empty() {
        parts.push(count(diff.restarts.len(), "unit", "units", "restarted"));
    }
    if !diff.params.is_empty() {
        parts.push(format!(
            "{} parameter change{}",
            diff.params.len(),
            if diff.params.len() == 1 { "" } else { "s" }
        ));
    }
    if !diff.refreshes.is_empty() {
        parts.push(count(
            diff.refreshes.len(),
            "manifest",
            "manifests",
            "refreshed",
        ));
    }
    if !diff.grant_adds.is_empty() {
        parts.push(count(diff.grant_adds.len(), "grant", "grants", "added"));
    }
    if !diff.grant_removes.is_empty() {
        parts.push(count(
            diff.grant_removes.len(),
            "grant",
            "grants",
            "removed",
        ));
    }
    parts.join(", ")
}

/// One feed or declared source row: the naming entity and its name for the
/// contributor, then the contributor's aspect, key and owner.
fn render_contributor(entity: &str, name: &str, contributor: &Contributor, out: &mut String) {
    out.push_str(&format!(
        "  {entity}.{name}  <-  {}.{}  ({}, owner={})\n",
        contributor.entity, contributor.aspect, contributor.key, contributor.owner
    ));
}

fn render_grant(grant: &Grant, mark: char, out: &mut String) {
    let prefix = if mark == ' ' {
        "  ".to_string()
    } else {
        format!("  {mark} ")
    };
    match (&grant.capability, grant.priority) {
        (Some(capability), Some(priority)) if grant.hold => out.push_str(&format!(
            "{prefix}{}.{}  holds  capability={capability}  priority={priority}{}\n",
            grant.unit,
            grant.publish,
            grant
                .on_loss
                .map_or_else(String::new, |on_loss| format!("  on_loss={on_loss}"))
        )),
        (Some(capability), Some(priority)) => out.push_str(&format!(
            "{prefix}{}.{}  capability={capability}  priority={priority}\n",
            grant.unit, grant.publish
        )),
        _ => out.push_str(&format!(
            "{prefix}{}.{}  binds\n",
            grant.unit, grant.publish
        )),
    }
    for key in &grant.keys {
        out.push_str(&format!("    key: {key}\n"));
    }
    let width = grant
        .entities
        .iter()
        .map(|e| e.name.len())
        .max()
        .unwrap_or(0);
    for entity in &grant.entities {
        out.push_str(&format!(
            "    -> {:width$}  (room={}, capability={}, write={}, owner={})\n",
            entity.name, entity.room, entity.capability, entity.write, entity.owner,
        ));
    }
}

fn render_unit(check: &CheckResult, unit: &LoadedUnit, out: &mut String) {
    let name = &unit.manifest.unit.name;
    out.push_str(&format!(
        "+ {} {} ({})\n",
        unit.manifest.unit.kind, name, unit.path
    ));
    out.push_str(&format!("    command: {}\n", unit.manifest.runtime.command));

    let entities: Vec<_> = check
        .house
        .entities
        .iter()
        .filter(|e| &e.owner == name)
        .collect();
    // Adapters always render the block, because an adapter with no entities
    // is worth noticing. Other binding units (automations with virtual
    // sensors) render it only when non-empty.
    if unit.manifest.unit.kind == UnitKind::Adapter || !entities.is_empty() {
        out.push_str(&format!("    entities ({}):\n", entities.len()));
        let name_w = entities.iter().map(|e| e.name.len()).max().unwrap_or(0);
        let cap_w = entities
            .iter()
            .map(|e| e.file.entity.capability.len())
            .max()
            .unwrap_or(0);
        let room_w = entities
            .iter()
            .map(|e| e.file.entity.room.len() + 5)
            .max()
            .unwrap_or(0);
        for entity in entities {
            out.push_str(&format!(
                "      {:name_w$}  {:cap_w$}  {:room_w$}  write={}",
                entity.name,
                entity.file.entity.capability,
                format!("room={}", entity.file.entity.room),
                entity.file.write_policy.mode(),
            ));
            // The family policy, per aspect (docs/design.md#family-policy).
            for (aspect, values) in &entity.file.write_policy.family {
                let entries: Vec<String> = values.iter().map(|(v, h)| format!("{v}={h}")).collect();
                out.push_str(&format!("  family.{aspect}: {}", entries.join(", ")));
            }
            if let Some(causes) = &entity.file.causes {
                // How its adapter attributes a change it did not command
                // (docs/design.md#device-reported-causes).
                out.push_str("  causes:");
                if !causes.person.is_empty() {
                    out.push_str(&format!(" person={}", causes.person.join(",")));
                }
                if let Some(event) = &causes.event {
                    out.push_str(&format!(" event={event}"));
                }
                if causes.uncommanded.is_some() {
                    out.push_str(" uncommanded=person");
                }
            }
            out.push('\n');
            // A button's gesture actions (docs/design.md#gesture-actions).
            for (gesture, targets) in entity.file.actions.iter().flatten() {
                for (target, action) in targets {
                    out.push_str(&format!(
                        "        {gesture} \u{2192} {target}: {}\n",
                        describe_action(action)
                    ));
                }
            }
        }
    }

    if let Some(params) = &unit.manifest.params {
        out.push_str("    params:\n");
        for (param, spec) in params {
            let mut line = format!(
                "      {param}  type={}  default={}",
                spec.param_type,
                display_value(&spec.default)
            );
            if let Some(constraint) = &spec.constraint {
                let parts: Vec<String> = constraint
                    .iter()
                    .map(|(k, v)| format!("{k}={}", display_value(v)))
                    .collect();
                line.push_str(&format!("  constraint={{{}}}", parts.join(", ")));
            }
            if let Some(editable_by) = spec.editable_by {
                line.push_str(&format!("  editable_by={editable_by}"));
            }
            out.push_str(&line);
            out.push('\n');
        }
    }
}

fn render_expanded(key: &ExpandedKey, out: &mut String) {
    let mut header = format!(
        "  {} {} {}: {}",
        key.unit,
        key.direction.verb(),
        key.entry,
        key.source
    );
    if let Some(zone) = &key.zone {
        header.push_str(&format!(" (zone {zone})"));
    }
    out.push_str(&header);
    out.push('\n');

    let unchanged = key.exprs.len() == 1 && key.exprs[0].to_string() == key.source;
    if !unchanged {
        for expr in &key.exprs {
            out.push_str(&format!("    -> {expr}\n"));
        }
    }
}

/// One gesture action as the plan prints it: what is sent, then the hold.
fn describe_action(action: &crate::manifest::Action) -> String {
    let mut parts = Vec::new();
    if let Some(aspect) = &action.toggle {
        parts.push(format!("toggle {aspect}"));
    }
    if let Some(set) = &action.set {
        let values: Vec<String> = set
            .iter()
            .map(|(aspect, value)| format!("{aspect}={}", display_value(value)))
            .collect();
        parts.push(format!("set {}", values.join(" ")));
    }
    match action.hold.as_deref() {
        Some("release") if parts.is_empty() => parts.push("release".to_string()),
        Some(hold) => parts.push(format!("hold {hold}")),
        None => {}
    }
    if action.unlocks {
        parts.push("unlocks".to_string());
    }
    parts.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(a: &str, d: &str) -> (String, String) {
        (a.to_string(), d.to_string())
    }

    fn level_only(repo: &[u8], world: &[u8]) -> bool {
        param_level_only(
            parse_manifest(repo).as_ref(),
            parse_manifest(world).as_ref(),
        )
    }

    fn changes(repo: &[u8], world: &[u8]) -> Vec<String> {
        refresh_changes(
            parse_manifest(repo).as_ref(),
            parse_manifest(world).as_ref(),
        )
    }

    fn world_manifest(kind: &str, params: &str) -> Option<toml::Value> {
        let manifest = format!(
            "schema = 1\n[unit]\nname = \"porch\"\nkind = \"{kind}\"\n\
             [runtime]\ncommand = \"true\"\n{params}"
        );
        parse_manifest(manifest.as_bytes())
    }

    #[test]
    fn the_world_default_of_an_undeclared_enabled_is_true_for_an_automation() {
        // A pause must read as a live edit under an unchanged default, or
        // the next apply would undo it.
        assert_eq!(
            world_default(world_manifest("automation", "").as_ref(), ENABLED_PARAM),
            Some(Value::Bool(true))
        );
        assert_eq!(
            world_default(world_manifest("adapter", "").as_ref(), ENABLED_PARAM),
            None
        );
        let declared = "[params.enabled]\ntype = \"bool\"\ndefault = false\n";
        assert_eq!(
            world_default(
                world_manifest("automation", declared).as_ref(),
                ENABLED_PARAM
            ),
            Some(Value::Bool(false))
        );
    }

    #[test]
    fn ordered_respects_edges_then_kind_then_name() {
        let set: BTreeSet<String> = ["watcher", "beacon", "zed"]
            .iter()
            .map(std::string::ToString::to_string)
            .collect();
        let edges = vec![edge("beacon", "watcher")];
        let kind_of = |name: &str| match name {
            "beacon" | "zed" => 0,
            _ => 1,
        };
        assert_eq!(
            ordered(&set, &edges, kind_of),
            vec![
                "beacon".to_string(),
                "zed".to_string(),
                "watcher".to_string()
            ]
        );
    }

    /// An automation that binds a commandable virtual entity is an edge source
    /// like an adapter. It starts before the automation commanding it, even
    /// where kind and name order would put it second.
    #[test]
    fn ordered_puts_a_latch_owner_before_its_commander() {
        let set: BTreeSet<String> = ["buttons", "modes"]
            .iter()
            .map(std::string::ToString::to_string)
            .collect();
        let edges = vec![edge("modes", "buttons")];
        assert_eq!(
            ordered(&set, &edges, |_| 1),
            vec!["modes".to_string(), "buttons".to_string()]
        );
    }

    #[test]
    fn param_level_change_is_detected() {
        let old = br#"
schema = 1
[unit]
name = "probe"
kind = "automation"
[runtime]
command = "uv run units/probe.py"
restart = "on-failure"
[params.level]
type = "int"
default = 1
constraint = { min = 0, max = 10 }
"#;
        let new_default = String::from_utf8_lossy(old).replace("default = 1", "default = 5");
        assert!(level_only(new_default.as_bytes(), old));

        let new_command = String::from_utf8_lossy(old).replace("units/probe.py", "units/other.py");
        assert!(!level_only(new_command.as_bytes(), old));

        let new_type = String::from_utf8_lossy(old).replace("type = \"int\"", "type = \"float\"");
        assert!(!level_only(new_type.as_bytes(), old));

        let added_param = format!(
            "{}\n[params.extra]\ntype = \"bool\"\ndefault = false\n",
            String::from_utf8_lossy(old)
        );
        assert!(!level_only(added_param.as_bytes(), old));
    }

    #[test]
    fn refresh_changes_render_stripped_field_deltas() {
        let old = br#"
schema = 1
[unit]
name = "probe"
kind = "automation"
[runtime]
command = "uv run units/probe.py"
restart = "on-failure"
[params.level]
type = "int"
default = 1
constraint = { min = 0, max = 10 }
editable_by = "family"
"#;
        let tightened = String::from_utf8_lossy(old).replace("max = 10", "max = 5");
        assert!(level_only(tightened.as_bytes(), old));
        assert_eq!(
            changes(tightened.as_bytes(), old),
            vec!["level: constraint {max=10, min=0} -> {max=5, min=0}"]
        );

        let owner_only = String::from_utf8_lossy(old)
            .replace("editable_by = \"family\"", "editable_by = \"owner\"");
        assert_eq!(
            changes(owner_only.as_bytes(), old),
            vec!["level: editable_by family -> owner"]
        );

        let dropped = String::from_utf8_lossy(old).replace("editable_by = \"family\"\n", "");
        assert_eq!(
            changes(dropped.as_bytes(), old),
            vec!["level: editable_by family -> (none)"]
        );

        let comment_only = format!("{}\n# a comment\n", String::from_utf8_lossy(old));
        assert!(level_only(comment_only.as_bytes(), old));
        assert_eq!(changes(comment_only.as_bytes(), old), Vec::<String>::new());
    }
}
