use std::fs;
use std::path::{Path, PathBuf};

use homeostat::error::render_sorted;

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn valid_house_produces_no_errors_and_golden_plan() {
    let result = homeostat::check(&manifest_dir().join("examples/house"));
    assert!(
        result.errors.is_empty(),
        "examples/house should be valid, got:\n{}",
        render_sorted(&result.errors).join("\n")
    );
    assert!(
        result.warnings.is_empty(),
        "examples/house should have no warnings, got:\n{}",
        result.warnings.join("\n")
    );

    let plan = homeostat::plan::render(&result, "examples/house");
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
        .filter_map(|e| e.ok())
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
