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
    }
}
