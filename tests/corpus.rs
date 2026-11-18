use std::fs;
use std::path::{Path, PathBuf};

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
        result.warnings.join("\n")
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

/// Generic units run in many fixture houses, each with a copy of the
/// unit's manifest. The copies must agree on what the unit is, what it
/// touches and what it lets the owner set. How a house starts the unit
/// (`[runtime]`), where a static endpoint points and the description are
/// the house's own, so they are not compared.
///
/// The starter house is left out: its units are a release's, and a
/// manifest change on main reaches it with the scripts at the next
/// release. Units whose copies carry per-house bindings or naming (the
/// dashboard, buttons, ntfy, persons) are left out too, and so is the
/// corpus, whose houses are trimmed to the error each one shows.
#[test]
fn copies_of_a_generic_manifest_agree() {
    // A house that runs a variant of the unit on purpose.
    let variants = [
        // A stand-in that answers slowly.
        ("recorder", "house_restore_slow"),
        // Adds the cause reports and the hold subscriptions.
        ("esphome", "house_esphome_causes"),
    ];
    let houses: Vec<PathBuf> = fs::read_dir(manifest_dir().join("tests/fixtures"))
        .expect("read tests/fixtures")
        .map(|entry| entry.expect("fixture entry").path())
        .collect();
    let mut out_of_step = Vec::new();
    for (unit, reference) in [
        ("arbiter", REFERENCE),
        ("recorder", REFERENCE),
        ("esphome", REFERENCE),
        ("owntracks", REFERENCE),
        ("zigbee", REFERENCE),
        ("clock", "tests/fixtures/house_evening"),
        ("reflector", "tests/fixtures/house_apply"),
    ] {
        let file = format!("units/{unit}.toml");
        let expected = comparable(&manifest_dir().join(reference).join(&file));
        for house in &houses {
            let path = house.join(&file);
            let name = house.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !path.exists() || variants.contains(&(unit, name)) {
                continue;
            }
            if comparable(&path) != expected {
                out_of_step.push(
                    path.strip_prefix(manifest_dir())
                        .unwrap()
                        .display()
                        .to_string(),
                );
            }
        }
    }
    for (unit, house) in variants {
        assert!(
            manifest_dir()
                .join(format!("tests/fixtures/{house}/units/{unit}.toml"))
                .exists(),
            "the variant {house}/units/{unit}.toml is gone"
        );
    }
    assert!(
        out_of_step.is_empty(),
        "these manifests differ from their reference copy:\n{}",
        out_of_step.join("\n")
    );
}

/// The manifest at `path` without the parts a house chooses for itself.
fn comparable(path: &Path) -> toml::Table {
    let text = fs::read_to_string(path).expect("read manifest");
    let mut manifest: toml::Table = toml::from_str(&text).expect("manifest parses");
    manifest.remove("runtime");
    if let Some(toml::Value::Table(unit)) = manifest.get_mut("unit") {
        unit.remove("description");
    }
    if let Some(toml::Value::Table(discovery)) = manifest.get_mut("discovery") {
        discovery.remove("endpoint");
    }
    manifest
}
