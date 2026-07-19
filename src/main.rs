//! `stepci` — a native, Dockerless debugger for GitHub Actions workflows.
//!
//! v0 milestone M3: parse a workflow and *run* its `run:` steps natively,
//! evaluating `if:` conditions and interpolating `${{ }}`. The per-step diff and
//! interactive debugger loop land in later milestones (see `README.md`).

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

use stepci::exec::{self, RunOptions};
use stepci::parse;

/// A native, Dockerless debugger for GitHub Actions workflows.
#[derive(Parser)]
#[command(name = "stepci", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a workflow locally, executing each `run:` step natively.
    Run(RunArgs),
}

#[derive(Args)]
struct RunArgs {
    /// Path to the workflow file (e.g. `.github/workflows/ci.yml`).
    workflow: PathBuf,

    /// Only run this job (defaults to all jobs, in dependency order).
    #[arg(long)]
    job: Option<String>,

    /// Pause before every step (interactive step-through).
    #[arg(long, short = 's')]
    step: bool,

    /// Pause before steps with these ids (repeatable).
    #[arg(long = "break", value_name = "STEP_ID")]
    breakpoints: Vec<String>,
}

fn main() {
    if let Err(err) = try_main() {
        // `{:#}` renders the full anyhow context chain on one line.
        eprintln!("stepci: {err:#}");
        std::process::exit(1);
    }
}

fn try_main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Run(args) => run(args),
    }
}

fn run(args: RunArgs) -> Result<()> {
    let workflow = parse::parse_file(&args.workflow)?;

    let opts = RunOptions {
        job: args.job,
        workspace: std::env::current_dir().context("getting the current directory")?,
        step_all: args.step,
        breakpoints: args.breakpoints,
    };
    let code = exec::run_workflow(&workflow, &opts)?;
    std::process::exit(code);
}
