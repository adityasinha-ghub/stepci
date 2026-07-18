//! `stepci` — a native, Dockerless debugger for GitHub Actions workflows.
//!
//! v0 milestone M1: parse a workflow and print a summary of its jobs and steps.
//! The native executor, per-step diff, and debugger loop land in later milestones
//! (see `README.md`).

use anyhow::{Result, bail};
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

use stepci::model::{StepAction, Workflow};
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
    /// Run a workflow locally, pausing at each step to inspect what it changed.
    Run(RunArgs),
}

#[derive(Args)]
struct RunArgs {
    /// Path to the workflow file (e.g. `.github/workflows/ci.yml`).
    workflow: PathBuf,

    /// Only run this job (defaults to all jobs).
    #[arg(long)]
    job: Option<String>,

    /// Pause before these step ids (repeatable). Default: pause at every step.
    #[arg(long = "break", value_name = "STEP_ID")]
    breakpoints: Vec<String>,

    /// Run to completion without pausing (still records per-step diffs).
    #[arg(long)]
    no_pause: bool,
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

    if let Some(job) = &args.job
        && !workflow.jobs.contains_key(job)
    {
        let available = workflow.jobs.keys().cloned().collect::<Vec<_>>().join(", ");
        bail!("job `{job}` not found; available jobs: {available}");
    }

    print_summary(&workflow, args.job.as_deref());

    // The executor isn't wired up yet; be honest about what these flags will do.
    if !args.breakpoints.is_empty() || args.no_pause {
        eprintln!(
            "\nnote: --break/--no-pause take effect once the native executor lands (next milestone)."
        );
    }
    Ok(())
}

fn print_summary(workflow: &Workflow, only: Option<&str>) {
    match &workflow.name {
        Some(name) => println!("workflow: {name}"),
        None => println!("workflow: (unnamed)"),
    }

    for (id, job) in &workflow.jobs {
        if only.is_some_and(|f| f != id) {
            continue;
        }
        let runs_on = if job.runs_on.is_empty() {
            "unspecified".to_string()
        } else {
            job.runs_on.join(", ")
        };
        println!("\njob {id}  (runs-on: {runs_on})");
        for (i, step) in job.steps.iter().enumerate() {
            let n = i + 1;
            match &step.action {
                StepAction::Run { script } => {
                    let label = step.name.clone().unwrap_or_else(|| first_line(script));
                    println!("  {n:>2}. run   {label}");
                }
                // Show the name and the action when named; just the action when not,
                // so unnamed steps don't render as `uses X  → X`.
                StepAction::Uses { action, .. } => match &step.name {
                    Some(name) => println!("  {n:>2}. uses  {name}  → {action}"),
                    None => println!("  {n:>2}. uses  {action}"),
                },
            }
        }
    }
}

/// The first non-blank line of a script, for labeling an unnamed `run:` step.
fn first_line(script: &str) -> String {
    script
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}
