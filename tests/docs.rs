//! The docs agree with what they describe.
//!
//! Code, tests and docs cite the design and the adapter contract by
//! anchor (`docs/design.md#arbitrated-mode`). The citation tests fail
//! when a heading is renamed out from under a citation, and when a
//! citation names a section in prose or by number instead of by anchor.
//! The Key space table in the design lists the classes `src/keyspace.rs`
//! defines. The other tests check lists the docs copy from the code or
//! the files: the MCP tools, the capability names, the widget
//! descriptions and the vendored assets' hashes.

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

/// The text files that may cite the design or the adapter contract:
/// everything tracked under these roots, skipping vendored assets and
/// lockfiles.
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
                // Vendored map files (adapters/assets/README.md).
                let vendored = path.ends_with("adapters/assets/map");
                if !generated
                    && !vendored
                    && !matches!(name.as_str(), "target" | "__pycache__" | "node_modules")
                {
                    walk(&path, out);
                }
            } else if [".rs", ".py", ".js", ".html", ".md", ".sh", ".toml"]
                .iter()
                .any(|ext| name.ends_with(ext))
                && !name.ends_with(".lock")
                && name != "video-rtc.js"
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

/// Every citation of `docs/{doc}` that does not resolve: an anchor with
/// no heading, or a section named in prose (`{doc}, Name`) or by number
/// (`{doc}, §4`, `({doc}), section 5`), which a renamed or inserted
/// section silently breaks.
fn broken_citations(doc: &str) -> Vec<String> {
    let text = fs::read_to_string(repo().join("docs").join(doc)).expect("the cited doc");
    let known = anchors(&text);
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
            for (at, _) in line.match_indices(doc) {
                let rest = &line[at + doc.len()..];
                if let Some(anchor) = rest.strip_prefix('#') {
                    let anchor: String = anchor
                        .chars()
                        .take_while(|c| c.is_alphanumeric() || matches!(c, '-' | '_'))
                        .collect();
                    if !known.contains(&anchor) {
                        broken.push(format!("{rel}:{}: no heading for #{anchor}", n + 1));
                    }
                } else if [",", ":", "`,", "`:", " §", "), section"]
                    .iter()
                    .any(|p| rest.starts_with(p))
                {
                    broken.push(format!(
                        "{rel}:{}: cite a section of {doc} by anchor, not by name or number",
                        n + 1
                    ));
                }
            }
        }
    }
    broken
}

#[test]
fn design_citations_resolve() {
    let broken = broken_citations("design.md");
    assert!(broken.is_empty(), "{}", broken.join("\n"));
}

#[test]
fn adapter_contract_citations_resolve() {
    let broken = broken_citations("adapters.md");
    assert!(broken.is_empty(), "{}", broken.join("\n"));
}

fn read(path: &str) -> String {
    fs::read_to_string(repo().join(path)).expect(path)
}

/// The backticked names in `text` between the first `start` and the
/// next `end` after it.
fn backticked(text: &str, start: &str, end: &str) -> BTreeSet<String> {
    let from = text.find(start).expect(start) + start.len();
    let span = &text[from..][..text[from..].find(end).expect(end)];
    span.split('`')
        .skip(1)
        .step_by(2)
        .map(String::from)
        .collect()
}

#[test]
fn mcp_tool_lists_match_the_server() {
    let tools: BTreeSet<String> = homeostat::mcp::tools()
        .as_array()
        .expect("tools array")
        .iter()
        .map(|tool| tool["name"].as_str().expect("tool name").to_string())
        .collect();
    let lists = [
        ("README.md", "serves the tools", "over stdio"),
        (
            "examples/starter-house/README.md",
            "The surface is read-only:",
            "for the authoring contract",
        ),
        (
            "examples/starter-house/AGENTS.md",
            "says how to connect):",
            "Use it",
        ),
    ];
    for (path, start, end) in lists {
        assert_eq!(backticked(&read(path), start, end), tools, "{path}");
    }
    // design.md lists them as the first column of a table.
    let design = read("docs/design.md");
    let table: BTreeSet<String> = design
        .split("| Tool | Reads |\n|---|---|\n")
        .nth(1)
        .expect("the tool table")
        .lines()
        .take_while(|line| line.starts_with('|'))
        .map(|row| {
            row.split('`')
                .nth(1)
                .expect("a backticked tool")
                .to_string()
        })
        .collect();
    assert_eq!(table, tools, "docs/design.md");
}

#[test]
fn capability_lists_match_the_vocabulary() {
    let names: BTreeSet<String> = homeostat::manifest::VOCABULARY
        .iter()
        .map(|c| c.name.to_string())
        .collect();
    let design = read("docs/design.md");
    let listed = backticked(&design, "(`VOCABULARY` in `src/manifest.rs`):", "`unknown-");
    assert_eq!(listed, names, "docs/design.md");
    // The `capability` field's doc comment, as rendered into manifest.md.
    let manifest = read("docs/manifest.md");
    let listed = backticked(
        &manifest,
        "| `capability` | string | yes | One of:",
        "`unknown-",
    );
    assert_eq!(listed, names, "docs/manifest.md");
}

/// The (kind, description) pairs of `docs/widgets.md`'s sections.
fn widget_sections(widgets: &str) -> Vec<(String, String)> {
    widgets
        .split("\n## `")
        .skip(1)
        .map(|section| {
            let (kind, body) = section.split_once("`\n\n").expect("a heading");
            let description = body.lines().next().expect("a description");
            (kind.to_string(), description.to_string())
        })
        .collect()
}

/// `docs/widgets.md` is written by `scripts/widget_gallery.py`, which needs a
/// browser for the pictures. Its descriptions come from manifest.md's
/// `WidgetKind` list, so this checks them without one.
#[test]
fn widget_descriptions_match_the_manifest() {
    let manifest = read("docs/manifest.md");
    let list = manifest
        .split("### WidgetKind")
        .nth(1)
        .expect("a WidgetKind section");
    let kinds: Vec<(String, String)> = list
        .lines()
        .take_while(|line| !line.starts_with('#'))
        .filter_map(|line| line.strip_prefix("- `")?.split_once("` — "))
        .map(|(kind, description)| (kind.to_string(), description.to_string()))
        .collect();
    assert!(!kinds.is_empty());
    assert_eq!(
        widget_sections(&read("docs/widgets.md")),
        kinds,
        "docs/widgets.md is stale: run uv run --script scripts/widget_gallery.py"
    );
}

/// The files under `adapters/assets/map`, relative to `adapters/assets`.
fn vendored_map_files() -> BTreeSet<String> {
    fn walk(dir: &Path, out: &mut BTreeSet<String>) {
        for entry in fs::read_dir(dir).expect("adapters/assets/map").flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else {
                let assets = repo().join("adapters/assets");
                let rel = path.strip_prefix(assets).expect("under assets");
                out.insert(rel.to_string_lossy().to_string());
            }
        }
    }
    let mut out = BTreeSet::new();
    walk(&repo().join("adapters/assets/map"), &mut out);
    out
}

#[test]
fn vendored_asset_hashes_match_the_table() {
    let readme = read("adapters/assets/README.md");
    let table = readme
        .split("| file | source | sha256 |\n|---|---|---|\n")
        .nth(1)
        .expect("the vendored files table");
    let mut wrong = Vec::new();
    let mut listed = BTreeSet::new();
    for row in table.lines().take_while(|line| line.starts_with('|')) {
        let cells: Vec<&str> = row.split('|').map(str::trim).collect();
        let (file, sha256) = (cells[1], cells[3]);
        let bytes = fs::read(repo().join("adapters/assets").join(file)).expect(file);
        if homeostat::content::sha256_hex(&bytes) != sha256 {
            wrong.push(format!("{file}: sha256 differs from its row"));
        }
        listed.insert(file.to_string());
    }
    for file in vendored_map_files().difference(&listed) {
        wrong.push(format!("{file}: vendored but has no row"));
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
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
    assert_eq!(slug("3. State"), "3-state");
}

/// The Key space table has a row for each class in `keyspace::CLASSES`,
/// and no row for a class the core does not have.
#[test]
fn key_space_table_lists_the_classes() {
    let design = fs::read_to_string(repo().join("docs/design.md")).expect("docs/design.md");
    let section = design
        .split("\n## Key space\n")
        .nth(1)
        .and_then(|rest| rest.split("\n### ").next())
        .expect("a Key space section");
    let documented: BTreeSet<&str> = section
        .lines()
        .filter_map(|line| line.strip_prefix("| `home/"))
        .filter_map(|rest| rest.split(['/', '`']).next())
        .collect();
    let defined: BTreeSet<&str> = homeostat::keyspace::CLASSES
        .iter()
        .map(|c| c.name)
        .collect();
    assert_eq!(documented, defined);
}
