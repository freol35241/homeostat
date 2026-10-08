//! Links into `docs/design.md` resolve. Code, tests and docs cite the
//! design by anchor (`docs/design.md#arbitrated-mode`); this test fails
//! when a heading is renamed out from under a citation, and when a
//! citation names a section in prose instead of by anchor.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// GitHub's heading anchor: lowercase, spaces to `-`, other punctuation
/// dropped.
fn slug(heading: &str) -> String {
    heading
        .chars()
        .filter_map(|c| match c {
            ' ' | '-' => Some('-'),
            '_' => Some('_'),
            c if c.is_alphanumeric() => Some(c.to_ascii_lowercase()),
            _ => None,
        })
        .collect()
}

fn anchors(design: &str) -> BTreeSet<String> {
    design
        .lines()
        .filter(|line| line.starts_with('#'))
        .map(|line| slug(line.trim_start_matches('#').trim()))
        .collect()
}

/// The text files that may cite the design: everything tracked under
/// these roots, skipping vendored assets and lockfiles.
fn sources() -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if path.is_dir() {
                // The starter's units are generated copies of a release
                // (scripts/sync_starter.sh) and cite the design as that
                // release did; they follow at the next one.
                let generated = path.ends_with("examples/starter-house/units");
                if !generated && !matches!(name.as_str(), "target" | "__pycache__" | "node_modules")
                {
                    walk(&path, out);
                }
            } else if [".rs", ".py", ".js", ".html", ".md", ".sh", ".toml"]
                .iter()
                .any(|ext| name.ends_with(ext))
                && !name.ends_with(".lock")
                && !matches!(
                    name.as_str(),
                    "leaflet.js" | "protomaps-leaflet.js" | "video-rtc.js" | "design.md"
                )
            {
                out.push(path);
            }
        }
    }
    let root = repo();
    let mut out = Vec::new();
    for dir in [
        "src", "sdk", "adapters", "tests", "docs", "examples", "scripts",
    ] {
        walk(&root.join(dir), &mut out);
    }
    for file in ["README.md", "CLAUDE.md"] {
        out.push(root.join(file));
    }
    out
}

#[test]
fn design_citations_resolve() {
    let design = fs::read_to_string(repo().join("docs/design.md")).expect("docs/design.md");
    let known = anchors(&design);
    let mut broken = Vec::new();
    for path in sources() {
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let rel = path
            .strip_prefix(repo())
            .unwrap_or(&path)
            .display()
            .to_string();
        for (n, line) in text.lines().enumerate() {
            for (at, _) in line.match_indices("design.md") {
                let rest = &line[at + "design.md".len()..];
                if let Some(anchor) = rest.strip_prefix('#') {
                    let anchor: String = anchor
                        .chars()
                        .take_while(|c| c.is_alphanumeric() || matches!(c, '-' | '_'))
                        .collect();
                    if !known.contains(&anchor) {
                        broken.push(format!("{rel}:{}: no heading for #{anchor}", n + 1));
                    }
                } else if [",", ":", "`,", "`:"].iter().any(|p| rest.starts_with(p)) {
                    // Split so this line does not cite the design itself.
                    broken.push(format!(
                        concat!(
                            "{}:{}: cite a section by anchor (design.md",
                            "#...), not by name"
                        ),
                        rel,
                        n + 1
                    ));
                }
            }
        }
    }
    assert!(broken.is_empty(), "{}", broken.join("\n"));
}

#[test]
fn slugs_follow_github() {
    assert_eq!(slug("Agent surface (MCP)"), "agent-surface-mcp");
    assert_eq!(
        slug("Restoring a unit's own last value"),
        "restoring-a-units-own-last-value"
    );
    assert_eq!(
        slug("Capabilities, grants and write policy"),
        "capabilities-grants-and-write-policy"
    );
}
