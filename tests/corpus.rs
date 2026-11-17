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
        let expected = read_lines(&case.join("expected_errors.txt"));
        let result = homeostat::check(&case.join("house"));
        let actual = render_sorted(&result.errors);
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
        [&serde_json::json!({"locked": {"false": {"until": "23:00"}, "true": "none"}})]
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
