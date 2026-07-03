use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

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
        Command::Plan { path } => {
            let result = homeostat::check(&path);
            if !result.errors.is_empty() {
                for line in homeostat::error::render_sorted(&result.errors) {
                    eprintln!("{line}");
                }
                eprintln!("\nplan failed: {} error(s)", result.errors.len());
                return ExitCode::FAILURE;
            }
            print!("{}", homeostat::plan::render(&result, &path.display().to_string()));
            ExitCode::SUCCESS
        }
        Command::Up { path, listen } => {
            let result = homeostat::check(&path);
            if !result.errors.is_empty() {
                for line in homeostat::error::render_sorted(&result.errors) {
                    eprintln!("{line}");
                }
                eprintln!("\nup refused: {} error(s)", result.errors.len());
                return ExitCode::FAILURE;
            }
            let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
            match runtime.block_on(homeostat::supervisor::run(&result.house, &path, &listen)) {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("up failed: {err}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}
