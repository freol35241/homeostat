//! Git facts about a house repo, by shelling out — no libgit dependency.
//!
//! The house need not be the worktree root: a house in a subdirectory of a
//! larger repo takes that repo's HEAD, and only changes inside the house
//! subtree count toward dirty (docs/design.md, step 5b).

use std::path::Path;
use std::process::Command;

/// The enclosing repo's HEAD, suffixed `-dirty` when the house subtree has
/// uncommitted changes. None when `root` is not inside a git worktree, or
/// the repo has no commits.
///
/// Only the subtree counts: a sibling directory's edits are not this
/// house's business, so a house nested in a larger repo is neither dirtied
/// nor blocked by work elsewhere in it. A house AT the worktree root is
/// the same rule with an empty prefix — every path in the repo is inside it.
///
/// Entries under the house's own `plans/` never count toward dirty: a saved
/// pending plan is a review artifact of the commit it was planned against
/// and must not invalidate itself.
pub fn head_commit(root: &Path) -> Option<String> {
    let head = git(root, &["rev-parse", "HEAD"])?;
    // `status --porcelain` prints paths relative to the worktree root, so
    // the house's own plans/ carries the house's prefix within the repo.
    let plans = format!("{}plans/", git(root, &["rev-parse", "--show-prefix"])?);
    let dirty = git(root, &["status", "--porcelain", "--", "."])
        .map(|s| s.lines().any(|line| !under_plans(line, &plans)))?;
    Some(if dirty { format!("{head}-dirty") } else { head })
}

/// Whether a `status --porcelain` line's path is under the house's
/// `plans/`. A rename counts only when both sides are; quoted (unusual)
/// paths never match and so still count as dirty.
fn under_plans(line: &str, plans: &str) -> bool {
    line.get(3..)
        .map(|path| path.split(" -> ").all(|p| p.starts_with(plans)))
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

    /// A repo with a house at `sub/` and an unrelated sibling directory.
    fn repo(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("homeostat-gitinfo-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("sub/units")).unwrap();
        fs::create_dir_all(dir.join("sibling")).unwrap();
        fs::write(dir.join("sub/zones.toml"), "schema = 1\n").unwrap();
        fs::write(dir.join("sibling/stack.yml"), "services: {}\n").unwrap();
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
    fn house_in_a_subdirectory_takes_the_enclosing_repo_head() {
        let dir = repo("subdir");
        let head = head_commit(&dir).expect("root is a repo");
        assert_eq!(head_commit(&dir.join("sub")), Some(head));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_the_house_subtree_counts_toward_dirty() {
        let dir = repo("scope");
        let house = dir.join("sub");
        let clean = head_commit(&house).expect("head");
        assert!(!clean.ends_with("-dirty"));

        // A sibling's edits are not this house's business.
        fs::write(dir.join("sibling/stack.yml"), "services: {web: {}}\n").unwrap();
        fs::write(dir.join("untracked-at-root"), "x").unwrap();
        assert_eq!(head_commit(&house), Some(clean.clone()));
        // ...but the whole repo is dirty, so a house AT the root sees it.
        assert!(head_commit(&dir).expect("head").ends_with("-dirty"));

        // The house's own files do count.
        fs::write(house.join("zones.toml"), "schema = 1\n# edited\n").unwrap();
        assert_eq!(head_commit(&house), Some(format!("{clean}-dirty")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_saved_plan_does_not_dirty_its_own_house() {
        let dir = repo("plans");
        let house = dir.join("sub");
        let clean = head_commit(&house).expect("head");
        fs::create_dir_all(house.join("plans/pending")).unwrap();
        fs::write(house.join("plans/pending/x.plan"), "schema = 1\n").unwrap();
        assert_eq!(head_commit(&house), Some(clean));
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
