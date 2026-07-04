pub mod bus;
pub mod config;
pub mod content;
pub mod error;
pub mod expand;
pub mod gitinfo;
pub mod grants;
pub mod keyspace;
pub mod manifest;
pub mod mcp;
pub mod pending;
pub mod plan;
pub mod repo;
pub mod supervisor;
pub mod validate;
pub mod world;

use std::path::Path;

pub use error::ValidationError;

pub struct CheckResult {
    pub house: repo::House,
    pub expanded: Vec<expand::ExpandedKey>,
    pub grants: Vec<grants::Grant>,
    pub warnings: Vec<String>,
    pub errors: Vec<ValidationError>,
}

/// Loads a house repo and runs the full plan-time pipeline: load, validate,
/// expand, resolve grants. Errors accumulate across all stages.
pub fn check(root: &Path) -> CheckResult {
    let (house, mut errors) = repo::load(root);
    errors.extend(validate::validate(&house));
    let (expanded, expand_errors) = expand::expand(&house);
    errors.extend(expand_errors);
    let (grants, warnings, grant_errors) = grants::resolve(&house, &expanded);
    errors.extend(grant_errors);
    CheckResult {
        house,
        expanded,
        grants,
        warnings,
        errors,
    }
}
