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
pub fn head_commit(root: &Path) -> Option<String> {
    let toplevel = git(root, &["rev-parse", "--show-toplevel"])?;
    let toplevel = Path::new(&toplevel).canonicalize().ok()?;
    if toplevel != root.canonicalize().ok()? {
        return None;
    }
    let head = git(root, &["rev-parse", "HEAD"])?;
    let dirty = git(root, &["status", "--porcelain"]).map(|s| !s.is_empty())?;
    Some(if dirty { format!("{head}-dirty") } else { head })
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
