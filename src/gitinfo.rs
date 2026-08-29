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
    // core.quotePath would C-quote any non-ASCII path ("plans/hus-\303\245"),
    // which then fails the plans/ test below and lets a saved plan dirty
    // the very commit it was planned against.
    let dirty = git(root, &["-c", "core.quotePath=false", "status", "--porcelain"])
        .map(|s| s.lines().any(|line| !under_plans(line)))?;
    Some(if dirty { format!("{head}-dirty") } else { head })
}

/// Whether a `status --porcelain` line's path is under `plans/`. A rename
/// counts only when both sides are; paths git still quotes even with
/// quotePath off (a literal quote or newline in the name) never match and
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    /// A repo with the house at its root, plus a nested directory that
    /// must not pass for a house of its own.
    fn repo(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("homeostat-gitinfo-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("nested/units")).unwrap();
        fs::write(dir.join("zones.toml"), "schema = 1\n").unwrap();
        fs::write(dir.join("nested/zones.toml"), "schema = 1\n").unwrap();
        run(&dir, &["init", "-q", "-b", "main"]);
        run(&dir, &["add", "-A"]);
        run(&dir, &["commit", "-qm", "initial"]);
        dir
    }

    fn run(root: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["-c", "user.name=test", "-c", "user.email=test@example.com"])
            .args(args)
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?} failed");
    }

    #[test]
    fn a_nested_directory_does_not_inherit_the_enclosing_repo_head() {
        let dir = repo("nested");
        assert!(head_commit(&dir).is_some(), "the worktree root is a house");
        assert_eq!(
            head_commit(&dir.join("nested")),
            None,
            "a directory inside a repo is not a house repo"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn uncommitted_changes_mark_the_head_dirty() {
        let dir = repo("dirty");
        let clean = head_commit(&dir).expect("head");
        assert!(!clean.ends_with("-dirty"));
        fs::write(dir.join("zones.toml"), "schema = 1\n# edited\n").unwrap();
        assert_eq!(head_commit(&dir), Some(format!("{clean}-dirty")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_saved_plan_does_not_dirty_its_own_house() {
        let dir = repo("plans");
        let clean = head_commit(&dir).expect("head");
        fs::create_dir_all(dir.join("plans/pending")).unwrap();
        fs::write(dir.join("plans/pending/x.plan"), "schema = 1\n").unwrap();
        // Non-ASCII too: git C-quotes such paths unless quotePath is off,
        // and a quoted line would fail the plans/ test and dirty the house.
        fs::write(dir.join("plans/pending/hus-å.plan"), "schema = 1\n").unwrap();
        assert_eq!(head_commit(&dir), Some(clean));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn outside_a_repo_there_is_no_commit() {
        let dir = std::env::temp_dir().join(format!("homeostat-gitinfo-bare-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(head_commit(&dir), None);
        let _ = fs::remove_dir_all(&dir);
    }
}
