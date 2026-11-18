//! The homeostat core: the manifest model and its validation, key
//! expansion, the grant table, plan and apply, the process supervisor and
//! the MCP server. The `homeostat` binary is built on it, and the
//! integration tests drive it directly.
//!
//! Every command starts from [`check`], the plan-time pipeline: load the
//! house repo, expand its key expressions, validate it, and resolve its
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

pub use error::{ValidationError, Warning};

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
    pub warnings: Vec<Warning>,
    pub errors: Vec<ValidationError>,
}

/// Loads a house repo and runs the full plan-time pipeline: load, expand,
/// validate, resolve grants. Errors accumulate across all stages.
pub fn check(root: &Path) -> CheckResult {
    let mut diags = error::Diagnostics::default();
    let house = repo::load(root, &mut diags);
    let expanded = expand::expand(&house, &mut diags);
    validate::validate(&house, &expanded, &mut diags);
    let grants = grants::resolve(&house, &expanded, &mut diags);
    let feeds = grants::resolve_feeds(&house, &expanded, &mut diags);
    let sources = grants::resolve_sources(&house, &expanded, &mut diags);
    grants::check_aspects(&house, &expanded, &mut diags);
    let error::Diagnostics { errors, warnings } = diags;
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
