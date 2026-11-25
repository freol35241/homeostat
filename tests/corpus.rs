use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use homeostat::error::render_sorted;

/// The reference house: every unit kind and write policy, as manifests
/// without code. Its plan is pinned in `corpus/expected_plan.txt`.
const REFERENCE: &str = "tests/fixtures/house_reference";

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn valid_house_produces_no_errors_and_golden_plan() {
    let root = manifest_dir().join(REFERENCE);
    let result = homeostat::check(&root);
    assert!(
        result.errors.is_empty(),
        "{REFERENCE} should be valid, got:\n{}",
        render_sorted(&result.errors).join("\n")
    );
    assert!(
        result.warnings.is_empty(),
        "{REFERENCE} should have no warnings, got:\n{}",
        result
            .warnings
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    );

    let world = homeostat::plan::World::empty();
    let diff = homeostat::plan::diff(&result, &root, &world, false);
    let plan = homeostat::plan::render(&result, &diff, REFERENCE, &world);
    if bless() {
        write_golden(
            &manifest_dir().join("tests/corpus/expected_plan.txt"),
            &plan,
        );
        return;
    }
    let expected = include_str!("corpus/expected_plan.txt");
    assert_eq!(
        plan, expected,
        "plan output diverged from tests/corpus/expected_plan.txt"
    );
}

#[test]
fn invalid_corpus_produces_expected_error_lists() {
    let invalid_root = manifest_dir().join("tests/corpus/invalid");
    let mut cases: Vec<PathBuf> = fs::read_dir(&invalid_root)
        .expect("tests/corpus/invalid exists")
        .filter_map(std::result::Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    cases.sort();
    assert!(!cases.is_empty(), "invalid corpus is empty");

    let mut failures = Vec::new();
    for case in &cases {
        let name = case.file_name().unwrap().to_string_lossy().to_string();
        let result = homeostat::check(&case.join("house"));
        let actual = render_sorted(&result.errors);
        let golden = case.join("expected_errors.txt");
        if bless() {
            // An empty list would pass the strict test forever, whatever
            // the house does, so a case that finds no error is not blessed.
            assert!(!actual.is_empty(), "case {name} reports no error");
            write_golden(&golden, &format!("{}\n", actual.join("\n")));
            continue;
        }
        let expected = read_lines(&golden);
        if actual != expected {
            failures.push(format!(
                "case {name}:\n  expected:\n    {}\n  actual:\n    {}",
                expected.join("\n    "),
                actual.join("\n    ")
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} corpus case(s) failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Whether to write the actual output over the expected files instead of
/// comparing. `HOMEOSTAT_BLESS=1 cargo test --test corpus` regenerates
/// them after an intended change to the plan or to an error message. A
/// blessed run checks nothing against the goldens, so review the diff.
fn bless() -> bool {
    std::env::var("HOMEOSTAT_BLESS").as_deref() == Ok("1")
}

fn write_golden(path: &Path, text: &str) {
    fs::write(path, text).unwrap_or_else(|e| panic!("cannot write {}: {e}", path.display()));
}

fn read_lines(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .unwrap_or_else(|_| panic!("missing {}", path.display()))
        .lines()
        .map(|l| l.trim_end().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// The family policy reaches the arbiter in the owner's binding row of the
/// grant table, as the JSON the arbiter reads, and in no cmd row.
#[test]
fn the_family_policy_rides_the_owners_binding_row() {
    let result = homeostat::check(&manifest_dir().join(REFERENCE));
    let rows: Vec<serde_json::Value> = result
        .grants
        .iter()
        .filter(|g| g.entities.iter().any(|e| e.name == "front_door_lock"))
        .map(|g| serde_json::to_value(g).unwrap())
        .collect();
    let policies: Vec<&serde_json::Value> = rows
        .iter()
        .flat_map(|g| g["entities"].as_array().unwrap())
        .filter(|e| e["name"] == "front_door_lock")
        .filter_map(|e| e.get("family"))
        .collect();
    assert_eq!(
        policies,
        [&serde_json::json!({"locked": {"false": {"until": "next change"}, "true": "none"}})]
    );
    let owner_row = rows
        .iter()
        .find(|g| g["unit"] == "zigbee" && g["capability"].is_null())
        .unwrap();
    assert!(owner_row["entities"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["name"] == "front_door_lock" && e.get("family").is_some()));
}

/// Each generic adapter's reference manifest, `adapters/<unit>.toml`, by
/// the name of the script it runs.
fn reference_manifests() -> Vec<(String, toml::Table)> {
    let stems = |extension: &str| -> Vec<String> {
        let mut stems: Vec<String> = fs::read_dir(manifest_dir().join("adapters"))
            .expect("read adapters/")
            .map(|entry| entry.expect("adapters entry").path())
            .filter(|path| path.extension().is_some_and(|ext| ext == extension))
            .map(|path| path.file_stem().unwrap().to_string_lossy().into_owned())
            .collect();
        stems.sort();
        stems
    };
    let (scripts, manifests) = (stems("py"), stems("toml"));
    for manifest in &manifests {
        assert!(
            scripts.contains(manifest),
            "adapters/{manifest}.toml has no script beside it"
        );
    }
    scripts
        .into_iter()
        .map(|script| {
            let path = manifest_dir().join(format!("adapters/{script}.toml"));
            assert!(
                path.exists(),
                "adapters/{script}.py has no reference manifest beside it"
            );
            let manifest = parse(&path);
            (script, manifest)
        })
        .collect()
}

fn parse(path: &Path) -> toml::Table {
    let text = fs::read_to_string(path).expect("read manifest");
    toml::from_str(&text).unwrap_or_else(|e| panic!("{} does not parse: {e}", path.display()))
}

/// The release the starter house is a snapshot of: `SDK_TAG` in
/// `scripts/sync_starter.sh`.
fn starter_tag() -> String {
    let script = fs::read_to_string(manifest_dir().join("scripts/sync_starter.sh"))
        .expect("read scripts/sync_starter.sh");
    script
        .lines()
        .find_map(|line| line.strip_prefix("SDK_TAG=\"")?.strip_suffix('"'))
        .expect("SDK_TAG in scripts/sync_starter.sh")
        .to_string()
}

/// `adapters/<script>.toml` as it is at `tag`. None when this clone does
/// not have the tag, or the tag has no such file.
fn reference_at(tag: &str, script: &str) -> Option<toml::Table> {
    let output = Command::new("git")
        .arg("-C")
        .arg(manifest_dir())
        .args(["show", &format!("{tag}:adapters/{script}.toml")])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).expect("a UTF-8 manifest");
    Some(
        toml::from_str(&text)
            .unwrap_or_else(|e| panic!("adapters/{script}.toml at {tag} does not parse: {e}")),
    )
}

/// The reference manifests together form a valid house, with an empty
/// entities directory for each adapter that binds entities.
#[test]
fn reference_manifests_are_valid() {
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("reference_manifests");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("units")).expect("create the house");
    for (script, manifest) in reference_manifests() {
        if let Some(dir) = manifest
            .get("entities")
            .and_then(|e| e.get("dir"))
            .and_then(toml::Value::as_str)
        {
            fs::create_dir_all(root.join(dir)).expect("create the entities dir");
        }
        let name = manifest["unit"]["name"].as_str().expect("unit name");
        fs::copy(
            manifest_dir().join(format!("adapters/{script}.toml")),
            root.join(format!("units/{name}.toml")),
        )
        .expect("copy the manifest");
    }
    let result = homeostat::check(&root);
    assert!(
        result.errors.is_empty(),
        "the reference manifests are not a valid house:\n{}",
        render_sorted(&result.errors).join("\n")
    );
}

/// Every house that runs a generic adapter carries a copy of its
/// manifest: the fixtures, the reference house and the starter house. A
/// copy agrees with its reference manifest when:
///
/// - each `[bus]` entry it declares is in the reference under the same
///   name with the same value;
/// - each parameter it declares is in the reference with the same type,
///   constraint and `editable_by`;
/// - `[unit].kind` and `watches`, `[discovery]` `mode` and `service`, and
///   whether `[entities]` is present are the reference's.
///
/// A house may declare a subset, choose its own defaults, and run the
/// unit under another name. Its naming, entities directory, endpoint,
/// description and runtime are its own.
///
/// A copy is found by the script its `[runtime].command` runs. The
/// starter and `house_reference` name the zigbee2mqtt copy `zigbee.py`
/// (`scripts/sync_starter.sh`). The invalid corpus is left out, since each
/// case is broken on purpose.
///
/// The starter's scripts come from the release it pins (`SDK_TAG`), so
/// its manifests are compared with the reference manifests at that tag.
/// A starter copy is skipped when the tag has no reference for its unit,
/// as a release cut before reference manifests existed has none, or when
/// this clone does not have the tag. A shallow clone lacks it, and CI
/// fetches full history.
#[test]
fn copies_of_a_generic_manifest_agree() {
    // Names the starter and the reference house give adapters they rename.
    let renamed = [("zigbee", "zigbee2mqtt")];
    // A house that runs its own script under an adapter's name.
    let variants = [
        // A stand-in recorder that answers slowly.
        "tests/fixtures/house_restore_slow/units/recorder.toml",
    ];
    let starter = "examples/starter-house/";
    let tag = starter_tag();
    let references: Vec<(String, toml::Table)> = reference_manifests();
    let mut copies = Vec::new();
    for houses in ["tests/fixtures", "examples"] {
        for house in fs::read_dir(manifest_dir().join(houses)).expect("read houses") {
            let units = house.expect("house entry").path().join("units");
            let Ok(entries) = fs::read_dir(&units) else {
                continue;
            };
            copies.extend(
                entries
                    .map(|entry| entry.expect("units entry").path())
                    .filter(|path| path.extension().is_some_and(|ext| ext == "toml")),
            );
        }
    }
    copies.sort();
    let mut matched: Vec<String> = Vec::new();
    let mut out_of_step = Vec::new();
    for path in &copies {
        let rel = path
            .strip_prefix(manifest_dir())
            .unwrap()
            .display()
            .to_string();
        let copy = parse(path);
        let Some(script) = copy
            .get("runtime")
            .and_then(|r| r.get("command"))
            .and_then(toml::Value::as_str)
            .and_then(|command| command.split_whitespace().find(|w| w.ends_with(".py")))
            .and_then(|word| Path::new(word).file_stem())
            .and_then(|stem| stem.to_str())
        else {
            continue;
        };
        let script = renamed
            .iter()
            .find(|(from, _)| *from == script)
            .map_or(script, |(_, to)| to);
        let Some((_, reference)) = references.iter().find(|(s, _)| s == script) else {
            continue;
        };
        if variants.contains(&rel.as_str()) {
            continue;
        }
        let (reference, source) = if rel.starts_with(starter) {
            let Some(at_tag) = reference_at(&tag, script) else {
                continue;
            };
            (at_tag, format!("adapters/{script}.toml at {tag}"))
        } else {
            matched.push(script.to_string());
            (reference.clone(), format!("adapters/{script}.toml"))
        };
        for problem in disagreements(&copy, &reference) {
            out_of_step.push(format!("{rel}: {problem} ({source})"));
        }
    }
    for variant in variants {
        assert!(
            manifest_dir().join(variant).exists(),
            "the variant {variant} is gone"
        );
    }
    for (script, _) in &references {
        assert!(
            matched.contains(script),
            "no fixture runs adapters/{script}.py, so nothing checks its reference manifest"
        );
    }
    assert!(
        out_of_step.is_empty(),
        "these manifests differ from their reference manifest:\n{}",
        out_of_step.join("\n")
    );
}

/// What in `copy` the reference manifest does not declare the same way.
fn disagreements(copy: &toml::Table, reference: &toml::Table) -> Vec<String> {
    let name = |m: &toml::Table| m["unit"]["name"].as_str().unwrap_or("").to_string();
    let (copy_name, reference_name) = (name(copy), name(reference));
    let show =
        |value: Option<&toml::Value>| value.map_or("absent".to_string(), ToString::to_string);
    let mut problems = Vec::new();
    for (table, field) in [
        ("unit", "kind"),
        ("unit", "watches"),
        ("discovery", "mode"),
        ("discovery", "service"),
    ] {
        let (got, want) = (section(copy, &[table]), section(reference, &[table]));
        if got.get(field) != want.get(field) {
            problems.push(format!(
                "{table}.{field} is {}, the reference has {}",
                show(got.get(field)),
                show(want.get(field))
            ));
        }
    }
    if copy.contains_key("entities") != reference.contains_key("entities") {
        problems.push("[entities] is present in one and not the other".to_string());
    }
    for direction in ["subscribes", "publishes"] {
        for (binding, value) in &section(copy, &["bus", direction]) {
            match section(reference, &["bus", direction]).get(binding) {
                None => problems.push(format!("bus.{direction}.{binding} is not in the reference")),
                Some(expected) => {
                    let expected = own_unit(expected.clone(), &reference_name, &copy_name);
                    if &expected != value {
                        problems.push(format!(
                            "bus.{direction}.{binding} is {value}, the reference has {expected}"
                        ));
                    }
                }
            }
        }
    }
    let reference_params = section(reference, &["params"]);
    for (param, spec) in &section(copy, &["params"]) {
        let Some(expected) = reference_params.get(param) else {
            problems.push(format!("params.{param} is not in the reference"));
            continue;
        };
        for field in ["type", "constraint", "editable_by"] {
            if spec.get(field) != expected.get(field) {
                problems.push(format!(
                    "params.{param}.{field} is {}, the reference has {}",
                    show(spec.get(field)),
                    show(expected.get(field))
                ));
            }
        }
    }
    problems
}

/// The table at `path` in `manifest`, empty when it is absent.
fn section(manifest: &toml::Table, path: &[&str]) -> toml::Table {
    let mut table = manifest;
    for key in path {
        match table.get(*key).and_then(toml::Value::as_table) {
            Some(inner) => table = inner,
            None => return toml::Table::new(),
        }
    }
    table.clone()
}

/// A reference subscription or publish, as a copy running the unit under
/// the name `to` must declare it. Discovery, hold and arbitration keys
/// carry the unit's own name as their third segment, so that segment
/// changes from `from` to `to`. Other keys stay as they are.
fn own_unit(value: toml::Value, from: &str, to: &str) -> toml::Value {
    let rename = |key: &str| {
        let mut segments: Vec<&str> = key.split('/').collect();
        if matches!(
            segments.get(1),
            Some(&("discovery" | "hold" | "arbitration"))
        ) && segments.get(2) == Some(&from)
        {
            segments[2] = to;
        }
        segments.join("/")
    };
    match value {
        toml::Value::String(key) => toml::Value::String(rename(&key)),
        toml::Value::Table(mut spec) => {
            if let Some(toml::Value::String(key)) = spec.get("key") {
                let key = rename(key);
                spec.insert("key".into(), toml::Value::String(key));
            }
            toml::Value::Table(spec)
        }
        other => other,
    }
}

/// The reflector is a test unit that many fixture houses run. Every copy
/// of its manifest agrees with `house_apply`'s, apart from the
/// description and the command.
#[test]
fn copies_of_the_reflector_agree() {
    let comparable = |path: &Path| {
        let mut manifest = parse(path);
        for (table, field) in [("unit", "description"), ("runtime", "command")] {
            if let Some(toml::Value::Table(inner)) = manifest.get_mut(table) {
                inner.remove(field);
            }
        }
        manifest
    };
    let fixtures = manifest_dir().join("tests/fixtures");
    let expected = comparable(&fixtures.join("house_apply/units/reflector.toml"));
    let mut out_of_step: Vec<String> = fs::read_dir(&fixtures)
        .expect("read tests/fixtures")
        .map(|entry| {
            entry
                .expect("fixture entry")
                .path()
                .join("units/reflector.toml")
        })
        .filter(|path| path.exists() && comparable(path) != expected)
        .map(|path| {
            path.strip_prefix(manifest_dir())
                .unwrap()
                .display()
                .to_string()
        })
        .collect();
    out_of_step.sort();
    assert!(
        out_of_step.is_empty(),
        "these reflector manifests differ from house_apply's:\n{}",
        out_of_step.join("\n")
    );
}
