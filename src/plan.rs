//! The plan engine: diffs a validated house repo against the world as the
//! bus reports it (see docs/design.md, step 5b). No state file — desired
//! state is the repo, actual state is queryable, drift is impossible by
//! construction.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::Path;

use serde_json::Value;

use crate::config::default_value;
use crate::content;
use crate::expand::ExpandedKey;
use crate::grants::Grant;
use crate::manifest::UnitKind;
use crate::repo::LoadedUnit;
use crate::validate::display_value;
use crate::CheckResult;

/// Plan tiers per docs/design.md, derived mechanically from the diff, never
/// declared. Any grant-table delta escalates to structural.
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

/// One unit as the world reports it: what is actually running.
#[derive(Debug, Clone)]
pub struct WorldUnit {
    /// The manifest TOML as the supervisor loaded it.
    pub manifest: Vec<u8>,
    pub manifest_hash: String,
    pub files_hash: String,
}

/// The actual world, read from the bus (manifest hashes, current
/// parameters, resolved grants) — or empty when planning offline.
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
    pub fn empty() -> World {
        World { label: "empty".to_string(), ..World::default() }
    }
}

/// The unit's world entry as the repo currently describes it — what the
/// supervisor records at startup and after a successful apply step.
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

#[derive(Debug)]
pub struct Restart {
    pub name: String,
    pub reason: String,
    /// The manifest itself changed (not only unit files): the unit's key
    /// surface may have changed, so the plan renders its expansion.
    pub manifest_changed: bool,
}

#[derive(Debug)]
pub struct ParamChange {
    pub unit: String,
    pub param: String,
    pub live: Value,
    pub repo: Value,
}

/// A unit whose manifest changed at parameter level only (the fields
/// `param_level_only` strips): no restart, but the running world must
/// refresh — the config store re-enforces the new constraints and the
/// served meta manifest updates so plans and the dashboard see it.
#[derive(Debug)]
pub struct Refresh {
    pub name: String,
    /// Per-param deltas, rendered "param: field old -> new".
    pub changes: Vec<String>,
}

#[derive(Debug, Default)]
pub struct Diff {
    pub creates: Vec<String>,
    pub destroys: Vec<String>,
    pub restarts: Vec<Restart>,
    pub params: Vec<ParamChange>,
    pub refreshes: Vec<Refresh>,
    pub grant_adds: Vec<Grant>,
    pub grant_removes: Vec<Grant>,
}

impl Diff {
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
/// check result.
pub fn diff(check: &CheckResult, root: &Path, world: &World) -> Diff {
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

        let manifest_bytes = fs::read(root.join(&unit.path)).unwrap_or_default();
        let files = content::files_hash(
            root,
            unit,
            &check.house,
            content::uses_zone(name, &check.expanded),
        );
        let files_changed = files != world_unit.files_hash;
        let hash_changed = content::manifest_hash(&manifest_bytes) != world_unit.manifest_hash;
        let param_level = hash_changed && param_level_only(&manifest_bytes, &world_unit.manifest);
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
                changes: refresh_changes(&manifest_bytes, &world_unit.manifest),
            });
        }

        // Parameter diffs: live value vs repo default. One rule covers both
        // a changed default and live drift — the repo is the system of
        // record either way.
        if let Some(params) = &unit.manifest.params {
            for (param, spec) in params {
                let repo = default_value(spec);
                let Some(live) = world.params.get(&(name.clone(), param.clone())) else {
                    continue; // unknown to the running world: covered by restart
                };
                if live != &repo {
                    diff.params.push(ParamChange {
                        unit: name.clone(),
                        param: param.clone(),
                        live: live.clone(),
                        repo,
                    });
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

/// True when the two manifests differ only in parameter values — every
/// param's `default`/`constraint`/`editable_by` stripped, the rest equal.
/// Param add/remove or a type change is NOT parameter-level: the running
/// unit read its manifest at startup. An unparseable side is a change.
fn param_level_only(repo: &[u8], world: &[u8]) -> bool {
    let parse = |bytes: &[u8]| -> Option<toml::Value> {
        toml::from_str(std::str::from_utf8(bytes).ok()?).ok()
    };
    let (Some(mut a), Some(mut b)) = (parse(repo), parse(world)) else {
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
fn refresh_changes(repo: &[u8], world: &[u8]) -> Vec<String> {
    let params = |bytes: &[u8]| -> toml::value::Table {
        std::str::from_utf8(bytes)
            .ok()
            .and_then(|text| toml::from_str::<toml::Value>(text).ok())
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

/// The apply walk over units, ordered by the grant table (adapters before
/// the automations granted onto their entities), never declared. Parameter
/// writes are not steps: they happen first and restart nothing.
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
    // Kinds once per unit up front: kind_of runs per comparison inside
    // ordered, and the world's kind lives in unparsed manifest TOML.
    let stop_kinds: BTreeMap<String, u8> = stop_set
        .iter()
        .map(|name| {
            (name.clone(), world.units.get(name).and_then(world_kind_order).unwrap_or(3))
        })
        .collect();
    let mut stops = ordered(&stop_set, &old_edges, |name| stop_kinds[name]);
    stops.reverse();
    steps.extend(stops.into_iter().map(|unit| Step { unit, action: StepAction::Stop }));

    let mut start_set: BTreeSet<String> = diff.creates.iter().cloned().collect();
    start_set.extend(diff.restarts.iter().map(|r| r.name.clone()));
    let new_edges = grant_edges(&check.grants);
    let creates: BTreeSet<&String> = diff.creates.iter().collect();
    for unit in ordered(&start_set, &new_edges, |name| {
        check.house.unit(name).map(|u| kind_order(u.manifest.unit.kind)).unwrap_or(3)
    }) {
        let action = if creates.contains(&unit) { StepAction::Start } else { StepAction::Restart };
        steps.push(Step { unit, action });
    }
    steps
}

/// Edges (owner, dependent) from a grant table: the granted entities'
/// owner units must be up before the granting unit. The owner rides in
/// the grant itself, so edges hold even for entities the repo no longer
/// declares. Owners are adapters, or automations for commandable virtual
/// entities (docs/design.md, Commandable virtual entities).
fn grant_edges(grants: &[Grant]) -> Vec<(String, String)> {
    let mut edges = Vec::new();
    for grant in grants {
        for entity in &grant.entities {
            if entity.owner != grant.unit {
                edges.push((entity.owner.clone(), grant.unit.clone()));
            }
        }
    }
    edges
}

/// Topological order over `set` under `edges` (owner before dependent),
/// each layer sorted by (kind, name). A cyclic grant table is refused at
/// check time (`grant-cycle`), so one cannot reach a plan; a malformed
/// table still degrades to sorted order rather than looping.
fn ordered<F>(set: &BTreeSet<String>, edges: &[(String, String)], kind_of: F) -> Vec<String>
where
    F: Fn(&str) -> u8,
{
    let mut remaining: BTreeSet<String> = set.clone();
    let mut out = Vec::new();
    while !remaining.is_empty() {
        let mut layer: Vec<String> = remaining
            .iter()
            .filter(|unit| {
                !edges
                    .iter()
                    .any(|(a, d)| d == *unit && a != *unit && remaining.contains(a))
            })
            .cloned()
            .collect();
        if layer.is_empty() {
            layer = remaining.iter().cloned().collect();
        }
        layer.sort_by_key(|name| (kind_of(name), name.clone()));
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

fn world_kind(unit: &WorldUnit) -> Option<String> {
    let text = std::str::from_utf8(&unit.manifest).ok()?;
    let value: toml::Value = toml::from_str(text).ok()?;
    Some(value.get("unit")?.get("kind")?.as_str()?.to_string())
}

fn world_kind_order(unit: &WorldUnit) -> Option<u8> {
    Some(match world_kind(unit)?.as_str() {
        "adapter" => 0,
        "automation" => 1,
        "service" => 2,
        _ => 3,
    })
}

/// Renders the full plan text. Only call on an error-free check result.
pub fn render(check: &CheckResult, root: &Path, repo_label: &str, world: &World) -> String {
    let diff = diff(check, root, world);
    let mut out = String::new();
    let mut units: Vec<&LoadedUnit> = check.house.units.iter().collect();
    units.sort_by_key(|u| (kind_order(u.manifest.unit.kind), u.manifest.unit.name.clone()));

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
        out.push_str(&format!("\nUnits to destroy ({}):\n\n", diff.destroys.len()));
        for name in &diff.destroys {
            let kind = world
                .units
                .get(name)
                .and_then(world_kind)
                .unwrap_or_else(|| "unit".to_string());
            out.push_str(&format!("- {kind} {name}\n"));
        }
    }

    if !diff.restarts.is_empty() {
        out.push_str(&format!("\nUnits to restart ({}):\n\n", diff.restarts.len()));
        for restart in &diff.restarts {
            let unit = check.house.unit(&restart.name).expect("restart of a repo unit");
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
                "  ~ {}/{}  live={}  repo={}\n",
                change.unit, change.param, change.live, change.repo
            ));
        }
    }

    if !diff.refreshes.is_empty() {
        out.push_str(&format!("\nManifest refreshes ({}):\n\n", diff.refreshes.len()));
        for refresh in &diff.refreshes {
            let unit = check.house.unit(&refresh.name).expect("refresh of a repo unit");
            out.push_str(&format!("  ~ {} ({})\n", refresh.name, unit.path));
            for change in &refresh.changes {
                out.push_str(&format!("      {change}\n"));
            }
        }
    }

    // Created units and units restarting on a manifest change: both may
    // bring new key surface, and an approval prompt must show it.
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
                out.push_str(&format!(
                    "  - {}.{}  capability={}  priority={}  -> {}\n",
                    grant.unit,
                    grant.publish,
                    grant.capability,
                    grant.priority,
                    grant
                        .entities
                        .iter()
                        .map(|e| e.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                ));
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

    // Device feeds: printed only when the house wires any, so a house
    // without them renders exactly as before.
    if !check.feeds.is_empty() {
        out.push_str("\nFeeds:\n\n");
        for feed in &check.feeds {
            out.push_str(&format!(
                "  {}.{}  <-  {}.{}  ({}, owner={})\n",
                feed.entity, feed.input, feed.source_entity, feed.source_aspect, feed.key, feed.source_owner
            ));
        }
    }

    if !check.warnings.is_empty() {
        out.push_str("\nWarnings:\n\n");
        for warning in &check.warnings {
            out.push_str(&format!("  {warning}\n"));
        }
    }

    if diff.is_empty() {
        out.push_str("\nNo changes. The world matches the repo.\n");
    } else {
        out.push_str(&format!(
            "\nPlan tier: {} ({})\n",
            derive_tier(&diff),
            summarize(&diff)
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
        parts.push(count(diff.refreshes.len(), "manifest", "manifests", "refreshed"));
    }
    if !diff.grant_adds.is_empty() {
        parts.push(count(diff.grant_adds.len(), "grant", "grants", "added"));
    }
    if !diff.grant_removes.is_empty() {
        parts.push(count(diff.grant_removes.len(), "grant", "grants", "removed"));
    }
    parts.join(", ")
}

fn render_grant(grant: &Grant, mark: char, out: &mut String) {
    let prefix = if mark == ' ' { "  ".to_string() } else { format!("  {mark} ") };
    out.push_str(&format!(
        "{prefix}{}.{}  capability={}  priority={}\n",
        grant.unit, grant.publish, grant.capability, grant.priority
    ));
    let width = grant.entities.iter().map(|e| e.name.len()).max().unwrap_or(0);
    for entity in &grant.entities {
        out.push_str(&format!(
            "    -> {:width$}  (room={}, write={}, owner={})\n",
            entity.name, entity.room, entity.write, entity.owner,
        ));
    }
}

fn render_unit(check: &CheckResult, unit: &LoadedUnit, out: &mut String) {
    let name = &unit.manifest.unit.name;
    out.push_str(&format!("+ {} {} ({})\n", unit.manifest.unit.kind, name, unit.path));
    out.push_str(&format!("    command: {}\n", unit.manifest.runtime.command));

    let entities: Vec<_> = check
        .house
        .entities
        .iter()
        .filter(|e| &e.owner == name)
        .collect();
    // Adapters always render the block (an empty one is telling); other
    // binding units (automations with virtual sensors) only when non-empty.
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
                "      {:name_w$}  {:cap_w$}  {:room_w$}  write={}\n",
                entity.name,
                entity.file.entity.capability,
                format!("room={}", entity.file.entity.room),
                entity.file.write_policy.mode,
            ));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(a: &str, d: &str) -> (String, String) {
        (a.to_string(), d.to_string())
    }

    #[test]
    fn ordered_respects_edges_then_kind_then_name() {
        let set: BTreeSet<String> =
            ["watcher", "beacon", "zed"].iter().map(|s| s.to_string()).collect();
        let edges = vec![edge("beacon", "watcher")];
        let kind_of = |name: &str| match name {
            "beacon" => 0,
            "zed" => 0,
            _ => 1,
        };
        assert_eq!(
            ordered(&set, &edges, kind_of),
            vec!["beacon".to_string(), "zed".to_string(), "watcher".to_string()]
        );
    }

    /// An automation that binds a commandable virtual entity is an edge
    /// source like an adapter: it starts before the automation commanding
    /// it, even where kind and name order would put it second.
    #[test]
    fn ordered_puts_a_latch_owner_before_its_commander() {
        let set: BTreeSet<String> = ["buttons", "modes"].iter().map(|s| s.to_string()).collect();
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
        assert!(param_level_only(new_default.as_bytes(), old));

        let new_command =
            String::from_utf8_lossy(old).replace("units/probe.py", "units/other.py");
        assert!(!param_level_only(new_command.as_bytes(), old));

        let new_type = String::from_utf8_lossy(old).replace("type = \"int\"", "type = \"float\"");
        assert!(!param_level_only(new_type.as_bytes(), old));

        let added_param = format!(
            "{}\n[params.extra]\ntype = \"bool\"\ndefault = false\n",
            String::from_utf8_lossy(old)
        );
        assert!(!param_level_only(added_param.as_bytes(), old));
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
        assert!(param_level_only(tightened.as_bytes(), old));
        assert_eq!(
            refresh_changes(tightened.as_bytes(), old),
            vec!["level: constraint {max=10, min=0} -> {max=5, min=0}"]
        );

        let owner_only =
            String::from_utf8_lossy(old).replace("editable_by = \"family\"", "editable_by = \"owner\"");
        assert_eq!(
            refresh_changes(owner_only.as_bytes(), old),
            vec!["level: editable_by family -> owner"]
        );

        let dropped = String::from_utf8_lossy(old).replace("editable_by = \"family\"\n", "");
        assert_eq!(
            refresh_changes(dropped.as_bytes(), old),
            vec!["level: editable_by family -> (none)"]
        );

        let comment_only = format!("{}\n# a comment\n", String::from_utf8_lossy(old));
        assert!(param_level_only(comment_only.as_bytes(), old));
        assert_eq!(refresh_changes(comment_only.as_bytes(), old), Vec::<String>::new());
    }
}
