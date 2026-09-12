use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use homeostat::bus::{ApplyRequest, ApplyResult};
use homeostat::plan::World;
use homeostat::CheckResult;

#[derive(Parser)]
#[command(name = "homeostat", version, about = "A household regulator")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Diff a house repo against the world and print the plan.
    Plan {
        /// Path to the house repo.
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Bus endpoint of the running supervisor (or HOMEOSTAT_BUS).
        /// Without one, the plan runs offline against the empty world.
        #[arg(long)]
        bus: Option<String>,
        /// Save the plan as a pending-plan file under plans/pending/.
        #[arg(long)]
        save: bool,
        /// Actor recorded in a saved pending plan.
        #[arg(long, default_value = "owner")]
        actor: String,
    },
    /// Plan against the live world, then command the supervisor to apply.
    Apply {
        /// Path to the house repo.
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Bus endpoint of the running supervisor (or HOMEOSTAT_BUS).
        #[arg(long)]
        bus: Option<String>,
        /// Apply a saved pending plan; refused when its base commit is no
        /// longer the repo's HEAD.
        #[arg(long)]
        plan: Option<PathBuf>,
    },
    /// Serve the agent surface: a read-only MCP server bound to a live
    /// house bus. Stdio by default (an MCP client launches it); --http for
    /// the deployed house, where it runs as a supervised service unit.
    Mcp {
        /// Bus endpoint of the running supervisor (or HOMEOSTAT_BUS).
        #[arg(long)]
        bus: Option<String>,
        /// Serve MCP over HTTP on this address (e.g. 127.0.0.1:8642)
        /// instead of stdio.
        #[arg(long)]
        http: Option<String>,
    },
    /// Print the manifest contract: JSON Schema for one file kind (all
    /// three without an argument), or the Markdown reference.
    Schema {
        /// unit | entity | zones
        file: Option<String>,
        /// Render docs/manifest.md instead of JSON.
        #[arg(long)]
        markdown: bool,
    },
    /// Explain a validation error code (all of them without an argument).
    Explain {
        /// The code from `error[<code>]`, e.g. state-publish-unbound.
        code: Option<String>,
    },
    /// Validate a house repo, then run its units under supervision.
    Up {
        /// Path to the house repo.
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Zenoh endpoint the supervisor listens on; units connect here.
        #[arg(long, default_value = homeostat::bus::DEFAULT_LISTEN)]
        listen: String,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Plan { path, bus, save, actor } => plan_command(path, bus, save, actor),
        Command::Apply { path, bus, plan } => apply_command(path, bus, plan),
        Command::Mcp { bus, http } => mcp_command(bus, http),
        Command::Explain { code } => explain_command(code),
        Command::Schema { file, markdown } => schema_command(file, markdown),
        Command::Up { path, listen } => {
            let Some(result) = checked(&path, "up") else {
                return ExitCode::FAILURE;
            };
            let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
            match runtime.block_on(homeostat::supervisor::run(&result, &path, &listen)) {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("up failed: {err}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}

/// Validates the repo; on errors prints them and returns None.
fn checked(path: &PathBuf, verb: &str) -> Option<CheckResult> {
    let result = homeostat::check(path);
    if !result.errors.is_empty() {
        let lines = homeostat::error::render_sorted(&result.errors);
        let count = lines.len();
        for line in lines {
            eprintln!("{line}");
        }
        eprintln!();
        for line in homeostat::error::explanations(&result.errors) {
            eprintln!("{line}");
        }
        eprintln!("\n{verb} refused: {count} error(s)");
        return None;
    }
    Some(result)
}

fn schema_command(file: Option<String>, markdown: bool) -> ExitCode {
    use homeostat::schema;
    if markdown {
        print!("{}", schema::markdown());
        return ExitCode::SUCCESS;
    }
    let value = match file {
        None => schema::all(),
        Some(name) => match schema::File::parse(&name) {
            Some(file) => schema::json(file),
            None => {
                eprintln!("unknown file kind \"{name}\": expected unit, entity or zones");
                return ExitCode::FAILURE;
            }
        },
    };
    println!("{}", serde_json::to_string_pretty(&value).expect("schema serializes"));
    ExitCode::SUCCESS
}

fn explain_command(code: Option<String>) -> ExitCode {
    match code {
        Some(code) => match homeostat::error::explain(&code) {
            Some(text) => {
                println!("{code}: {text}");
                ExitCode::SUCCESS
            }
            None => {
                eprintln!("unknown error code \"{code}\"; `homeostat explain` lists them");
                ExitCode::FAILURE
            }
        },
        None => {
            for (code, text) in homeostat::error::CODES {
                println!("{code}: {text}\n");
            }
            ExitCode::SUCCESS
        }
    }
}

/// The endpoint from --bus, falling back to HOMEOSTAT_BUS.
fn endpoint(flag: Option<String>) -> Option<String> {
    flag.or_else(|| std::env::var(homeostat::bus::ENV_BUS).ok())
        .filter(|e| !e.is_empty())
}

fn plan_command(path: PathBuf, bus: Option<String>, save: bool, actor: String) -> ExitCode {
    let Some(result) = checked(&path, "plan") else {
        return ExitCode::FAILURE;
    };
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let world = match endpoint(bus) {
        Some(endpoint) => {
            let read = runtime.block_on(async {
                let session = homeostat::world::connect(&endpoint).await?;
                let world = homeostat::world::read(&session, &endpoint).await;
                let _ = session.close().await;
                world
            });
            match read {
                Ok(world) => world,
                Err(err) => {
                    eprintln!("plan failed: {err}");
                    return ExitCode::FAILURE;
                }
            }
        }
        None if save => {
            eprintln!("plan --save needs the live world: pass --bus or set HOMEOSTAT_BUS");
            return ExitCode::FAILURE;
        }
        None => World::empty(),
    };

    let text = homeostat::plan::render(&result, &path, &path.display().to_string(), &world);
    print!("{text}");

    if save {
        let diff = homeostat::plan::diff(&result, &path, &world);
        if diff.is_empty() {
            eprintln!("nothing to save: the world matches the repo");
            return ExitCode::FAILURE;
        }
        let Some(base_commit) = homeostat::gitinfo::head_commit(&path) else {
            eprintln!(
                "plan --save needs a base commit: {} is not a git repository root",
                path.display()
            );
            return ExitCode::FAILURE;
        };
        let tier = homeostat::plan::derive_tier(&diff).to_string();
        match homeostat::pending::save(&path, &text, &tier, &actor, &base_commit) {
            Ok(saved) => println!("\nPending plan saved: {}", saved.display()),
            Err(err) => {
                eprintln!("plan --save failed: {err}");
                return ExitCode::FAILURE;
            }
        }
    }
    ExitCode::SUCCESS
}

fn mcp_command(bus: Option<String>, http: Option<String>) -> ExitCode {
    let Some(endpoint) = endpoint(bus) else {
        eprintln!("mcp needs a running supervisor: pass --bus or set HOMEOSTAT_BUS");
        return ExitCode::FAILURE;
    };
    let server = match homeostat::mcp::Server::start(&endpoint) {
        Ok(server) => server,
        Err(err) => {
            eprintln!("mcp failed: {err}");
            return ExitCode::FAILURE;
        }
    };
    let served = match http {
        Some(addr) => homeostat::mcp::http::serve(std::sync::Arc::new(server), &addr),
        None => homeostat::mcp::protocol::serve_stdio(&server),
    };
    match served {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("mcp failed: {err}");
            ExitCode::FAILURE
        }
    }
}

fn apply_command(path: PathBuf, bus: Option<String>, plan_file: Option<PathBuf>) -> ExitCode {
    let Some(endpoint) = endpoint(bus) else {
        eprintln!("apply needs a running supervisor: pass --bus or set HOMEOSTAT_BUS");
        return ExitCode::FAILURE;
    };

    // A pending plan auto-invalidates when the repo moves past its base
    // commit; on a match the plan is recomputed fresh below — the file is
    // a review artifact, not an execution script.
    if let Some(plan_file) = &plan_file {
        let pending = match homeostat::pending::load(plan_file) {
            Ok(pending) => pending,
            Err(err) => {
                eprintln!("apply refused: {err}");
                return ExitCode::FAILURE;
            }
        };
        let Some(head) = homeostat::gitinfo::head_commit(&path) else {
            eprintln!(
                "apply --plan refused: {} is not a git repository root, \
                 so the plan's base commit cannot be checked",
                path.display()
            );
            return ExitCode::FAILURE;
        };
        if pending.base_commit != head {
            eprintln!(
                "apply refused: pending plan {} is stale \
                 (base commit {} but HEAD is {head})",
                pending.id, pending.base_commit
            );
            return ExitCode::FAILURE;
        }
    }

    let Some(result) = checked(&path, "apply") else {
        return ExitCode::FAILURE;
    };
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    runtime.block_on(async {
        let session = match homeostat::world::connect(&endpoint).await {
            Ok(session) => session,
            Err(err) => {
                eprintln!("apply failed: {err}");
                return ExitCode::FAILURE;
            }
        };
        let world = match homeostat::world::read(&session, &endpoint).await {
            Ok(world) => world,
            Err(err) => {
                eprintln!("apply failed: {err}");
                return ExitCode::FAILURE;
            }
        };
        let diff = homeostat::plan::diff(&result, &path, &world);
        print!(
            "{}",
            homeostat::plan::render(&result, &path, &path.display().to_string(), &world)
        );
        if diff.is_empty() {
            return ExitCode::SUCCESS;
        }

        let request = ApplyRequest {
            base_commit: homeostat::gitinfo::head_commit(&path),
        };
        println!("\nApplying...");
        let planned_steps = diff.destroys.len() + diff.creates.len() + diff.restarts.len();
        let (outcome, replied_ok) =
            match homeostat::bus::request_apply(&session, &request, planned_steps).await {
                Ok(result) => result,
                Err(err) => {
                    eprintln!("apply failed: {err}");
                    return ExitCode::FAILURE;
                }
            };
        let _ = session.close().await;
        render_outcome(&outcome, replied_ok)
    })
}

fn render_outcome(outcome: &ApplyResult, replied_ok: bool) -> ExitCode {
    if let Some(error) = &outcome.error {
        eprintln!("apply refused: {error}");
        return ExitCode::FAILURE;
    }
    for line in outcome.detail_lines() {
        println!("{line}");
    }
    if outcome.ok && replied_ok {
        println!("Applied.");
        ExitCode::SUCCESS
    } else {
        eprintln!("{}", outcome.halt_summary());
        ExitCode::FAILURE
    }
}
