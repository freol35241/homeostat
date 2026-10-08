//! The homeostat core: the manifest model and its validation, key
//! expansion, the grant table, plan and apply, the process supervisor and
//! the MCP server. The `homeostat` binary is built on it, and the
//! integration tests drive it directly.
//!
//! Every command starts from [`check`], the plan-time pipeline: load the
//! house repo, validate it, expand its key expressions, and resolve its
//! grants, feeds and sources.

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
pub mod schema;
pub mod supervisor;
pub mod validate;
pub mod world;

use std::path::Path;

pub use error::ValidationError;

/// Everything the plan-time pipeline learned about a house: the loaded
/// files, the expanded keys, the resolved grants, feeds and sources, and
/// every warning and error found on the way. A house with no errors may be
/// planned and applied.
pub struct CheckResult {
    pub house: repo::House,
    pub expanded: Vec<expand::ExpandedKey>,
    pub grants: Vec<grants::Grant>,
    pub feeds: Vec<grants::Feed>,
    pub sources: Vec<grants::Source>,
    pub warnings: Vec<String>,
    pub errors: Vec<ValidationError>,
}

/// Loads a house repo and runs the full plan-time pipeline: load, validate,
/// expand, resolve grants. Errors accumulate across all stages.
pub fn check(root: &Path) -> CheckResult {
    let (house, mut errors) = repo::load(root);
    errors.extend(validate::validate(&house));
    let (expanded, mut warnings, expand_errors) = expand::expand(&house);
    errors.extend(expand_errors);
    let (grants, grant_warnings, grant_errors) = grants::resolve(&house, &expanded);
    warnings.extend(grant_warnings);
    errors.extend(grant_errors);
    let (feeds, feed_errors) = grants::resolve_feeds(&house, &expanded);
    errors.extend(feed_errors);
    let (sources, source_warnings, source_errors) = grants::resolve_sources(&house, &expanded);
    warnings.extend(source_warnings);
    errors.extend(source_errors);
    CheckResult {
        house,
        expanded,
        grants,
        feeds,
        sources,
        warnings,
        errors,
    }
}
