//! Git facts about a house repo, by shelling out — no libgit dependency.
//!
//! `applied_commit` exists only when the house root is itself a git
//! worktree root: a nested fixture directory must not inherit an enclosing
//! repo's HEAD (docs/design.md, step 5b).

use std::path::Path;
use std::process::Command;

/// The repo's HEAD, suffixed `-dirty` when the worktree has uncommitted
/// changes. None when `root` is not itself the top level of a git worktree
/// (not a repo, a nested directory, or a repo without commits).
///
/// Entries under `plans/` never count toward dirty: a saved pending plan is
/// a review artifact of the commit it was planned against and must not
/// invalidate itself.
pub fn head_commit(root: &Path) -> Option<String> {
    let toplevel = git(root, &["rev-parse", "--show-toplevel"])?;
    let toplevel = Path::new(&toplevel).canonicalize().ok()?;
    if toplevel != root.canonicalize().ok()? {
        return None;
    }
    let head = git(root, &["rev-parse", "HEAD"])?;
    let dirty = git(root, &["status", "--porcelain"])
        .map(|s| s.lines().any(|line| !under_plans(line)))?;
    Some(if dirty { format!("{head}-dirty") } else { head })
}

/// Whether a `status --porcelain` line's path is under `plans/`. A rename
/// counts only when both sides are; quoted (unusual) paths never match and
/// so still count as dirty.
fn under_plans(line: &str) -> bool {
    line.get(3..)
        .map(|path| path.split(" -> ").all(|p| p.starts_with("plans/")))
        .unwrap_or(false)
}

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
