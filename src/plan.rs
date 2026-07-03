use std::fmt;

use crate::expand::ExpandedKey;
use crate::manifest::UnitKind;
use crate::repo::LoadedUnit;
use crate::validate::display_value;
use crate::CheckResult;

/// Plan tiers per DESIGN.md, derived mechanically from the diff, never
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

/// The actual world. In later steps this is queried from the bus (manifest
/// hashes, liveliness, current parameters); step 1 only knows the empty world.
pub struct World;

impl World {
    pub fn empty() -> World {
        World
    }
}

#[derive(Debug, Default)]
pub struct Diff {
    pub unit_creates: usize,
    pub unit_destroys: usize,
    pub grant_adds: usize,
    pub grant_removes: usize,
    pub behavioral_changes: usize,
    pub param_changes: usize,
}

pub fn derive_tier(diff: &Diff) -> Tier {
    if diff.unit_creates + diff.unit_destroys + diff.grant_adds + diff.grant_removes > 0 {
        Tier::Structural
    } else if diff.behavioral_changes > 0 {
        Tier::Behavioral
    } else {
        Tier::ParameterOnly
    }
}

/// Diffs the validated repo against the world. Against the empty world every
/// unit is a create and every grant is an add.
pub fn diff(check: &CheckResult, _world: &World) -> Diff {
    Diff {
        unit_creates: check.house.units.len(),
        grant_adds: check.grants.len(),
        ..Diff::default()
    }
}

fn kind_order(kind: UnitKind) -> u8 {
    match kind {
        UnitKind::Adapter => 0,
        UnitKind::Automation => 1,
        UnitKind::Service => 2,
    }
}

/// Renders the full plan text. Only call on an error-free check result.
pub fn render(check: &CheckResult, repo_label: &str) -> String {
    let diff = diff(check, &World::empty());
    let tier = derive_tier(&diff);
    let mut out = String::new();
    let mut units: Vec<&LoadedUnit> = check.house.units.iter().collect();
    units.sort_by_key(|u| (kind_order(u.manifest.unit.kind), u.manifest.unit.name.clone()));

    out.push_str("Homeostat plan\n");
    out.push_str(&format!("  repo:  {repo_label}\n"));
    out.push_str("  world: empty\n\n");

    out.push_str(&format!("Units to create ({}):\n\n", units.len()));
    for unit in &units {
        render_unit(check, unit, &mut out);
    }

    out.push_str("\nExpanded keys:\n\n");
    for unit in &units {
        let name = &unit.manifest.unit.name;
        for key in check.expanded.iter().filter(|k| &k.unit == name) {
            render_expanded(key, &mut out);
        }
    }

    out.push_str("\nGrant table:\n\n");
    if check.grants.is_empty() {
        out.push_str("  (none)\n");
    }
    for grant in &check.grants {
        out.push_str(&format!(
            "  {}.{}  capability={}  priority={}\n",
            grant.unit, grant.publish, grant.capability, grant.priority
        ));
        let width = grant.entities.iter().map(|e| e.len()).max().unwrap_or(0);
        for name in &grant.entities {
            let entity = check
                .house
                .entities
                .iter()
                .find(|e| &e.name == name)
                .expect("granted entity exists");
            out.push_str(&format!(
                "    -> {:width$}  (room={}, write={}, owner={})\n",
                name,
                entity.file.entity.room,
                entity.file.write_policy.mode,
                entity.file.write_policy.owner,
            ));
        }
    }

    if !check.warnings.is_empty() {
        out.push_str("\nWarnings:\n\n");
        for warning in &check.warnings {
            out.push_str(&format!("  {warning}\n"));
        }
    }

    out.push_str(&format!(
        "\nPlan tier: {tier} ({} {} created, {} {} added)\n",
        diff.unit_creates,
        if diff.unit_creates == 1 { "unit" } else { "units" },
        diff.grant_adds,
        if diff.grant_adds == 1 { "grant" } else { "grants" },
    ));

    out
}

fn render_unit(check: &CheckResult, unit: &LoadedUnit, out: &mut String) {
    let name = &unit.manifest.unit.name;
    out.push_str(&format!("+ {} {} ({})\n", unit.manifest.unit.kind, name, unit.path));
    out.push_str(&format!("    command: {}\n", unit.manifest.runtime.command));

    if unit.manifest.unit.kind == UnitKind::Adapter {
        let entities: Vec<_> = check
            .house
            .entities
            .iter()
            .filter(|e| &e.adapter == name)
            .collect();
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
