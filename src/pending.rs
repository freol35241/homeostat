//! Pending plans as files: `plans/pending/{id}.plan` — TOML carrying the
//! rendered plan text plus what apply needs to enforce staleness. Readable
//! on a phone as-is; auto-invalidated when the repo moves past the base
//! commit (docs/design.md, step 5b).

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct PendingPlan {
    pub schema: u32,
    pub id: String,
    pub actor: String,
    pub created: String,
    pub base_commit: String,
    pub tier: String,
    pub plan: String,
}

/// Writes a pending plan under `{root}/plans/pending/` and returns its path.
pub fn save(
    root: &Path,
    plan_text: &str,
    tier: &str,
    actor: &str,
    base_commit: &str,
) -> Result<PathBuf, String> {
    let (created, compact) = utc_now();
    let id = format!("{compact}-{tier}");
    let dir = root.join("plans/pending");
    fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let path = dir.join(format!("{id}.plan"));
    let mut plan = plan_text.to_string();
    if !plan.ends_with('\n') {
        plan.push('\n');
    }
    let content = format!(
        "schema = 1\n\
         id = \"{id}\"\n\
         actor = \"{actor}\"\n\
         created = \"{created}\"\n\
         base_commit = \"{base_commit}\"\n\
         tier = \"{tier}\"\n\
         plan = '''\n{plan}'''\n"
    );
    fs::write(&path, content).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(path)
}

pub fn load(path: &Path) -> Result<PendingPlan, String> {
    let text = fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    toml::from_str(&text).map_err(|e| format!("{} is not a pending plan: {e}", path.display()))
}

/// Current UTC time as (RFC3339, compact id form). Whole-second precision;
/// no date-time dependency for two format strings.
fn utc_now() -> (String, String) {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after 1970")
        .as_secs();
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (year, month, day) = civil_from_days(days as i64);
    (
        format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}Z"),
        format!("{year:04}{month:02}{day:02}T{h:02}{m:02}{s:02}Z"),
    )
}

/// Days since 1970-01-01 to (year, month, day). Howard Hinnant's
/// civil_from_days algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_907), (2024, 7, 3)); // leap year passed
        assert_eq!(civil_from_days(20_638), (2026, 7, 4));
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = std::env::temp_dir().join(format!("homeostat-pending-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = save(&dir, "Homeostat plan\n  repo: x\n", "behavioral", "owner", "abc123")
            .unwrap();
        let plan = load(&path).unwrap();
        assert_eq!(plan.schema, 1);
        assert_eq!(plan.actor, "owner");
        assert_eq!(plan.base_commit, "abc123");
        assert_eq!(plan.tier, "behavioral");
        assert!(plan.plan.contains("Homeostat plan"));
        let _ = fs::remove_dir_all(&dir);
    }
}
