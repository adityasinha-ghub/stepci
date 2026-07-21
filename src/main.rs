//! `stepci` — a native, Dockerless debugger for GitHub Actions workflows.
//!
//! Run a workflow locally and step through it (`stepci run`), then re-open a
//! finished run to see what each step changed (`stepci runs` / `stepci show`).

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

use stepci::exec::{self, RunOptions};
use stepci::parse;
use stepci::record;

/// A native, Dockerless debugger for GitHub Actions workflows.
#[derive(Parser)]
#[command(name = "stepci", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a workflow locally, executing each step natively.
    Run(RunArgs),
    /// List recent recorded runs (newest first).
    Runs,
    /// Re-open a recorded run and show what each step changed.
    Show {
        /// Which run to show — 1 is the most recent (the default).
        #[arg(default_value = "1")]
        run: usize,
    },
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

    /// Load secrets from a dotenv-style file (values may be `op://…`/`vault://…`).
    #[arg(long = "secret-file", value_name = "PATH")]
    secret_file: Option<PathBuf>,

    /// A secret as `NAME=VALUE`, or bare `NAME` to read it from the environment
    /// (repeatable). `op://…`/`vault://…` values are resolved via their CLIs.
    #[arg(long = "secret", value_name = "NAME[=VALUE]")]
    secrets: Vec<String>,

    /// Don't save this run as a re-openable recording.
    #[arg(long)]
    no_record: bool,
}

fn main() {
    if let Err(err) = try_main() {
        // `{:#}` renders the full anyhow context chain on one line.
        eprintln!("stepci: {err:#}");
        std::process::exit(1);
    }
}

fn try_main() -> Result<()> {
    match Cli::parse().command {
        Command::Run(args) => run(args),
        Command::Runs => list_runs(),
        Command::Show { run } => show_run(run),
    }
}

fn run(args: RunArgs) -> Result<()> {
    let workflow = parse::parse_file(&args.workflow)?;
    let secrets = stepci::secrets::load_secrets(args.secret_file.as_deref(), &args.secrets)?;

    // A per-invocation artifact store (pid-scoped, cleaned up after the run) so
    // `upload-artifact`/`download-artifact` pass files between jobs locally.
    let artifacts = std::env::temp_dir().join(format!("stepci-artifacts-{}", std::process::id()));

    let opts = RunOptions {
        job: args.job,
        workspace: std::env::current_dir().context("getting the current directory")?,
        step_all: args.step,
        breakpoints: args.breakpoints,
        secrets,
        artifacts,
        workflow: args.workflow.display().to_string(),
        record: !args.no_record,
    };
    let code = exec::run_workflow(&workflow, &opts)?;
    let _ = std::fs::remove_dir_all(&opts.artifacts);
    std::process::exit(code);
}

/// `stepci runs` — list recent recorded runs.
fn list_runs() -> Result<()> {
    let runs = record::list()?;
    if runs.is_empty() {
        println!("No recorded runs yet — run `stepci run <workflow>` first.");
        return Ok(());
    }
    for (i, r) in runs.iter().enumerate() {
        let mark = if r.exit_code == 0 { "✓" } else { "✗" };
        let steps: usize = r.jobs.iter().map(|j| j.steps.len()).sum();
        println!(
            "  #{:<3} {mark}  {}  ({}, {} job{}, {} step{})",
            i + 1,
            r.workflow,
            relative_time(r.started_unix_ms),
            r.jobs.len(),
            plural(r.jobs.len()),
            steps,
            plural(steps),
        );
    }
    println!("\nRe-open one with `stepci show <n>` (default: the most recent).");
    Ok(())
}

/// `stepci show <n>` — re-open a recorded run and replay its per-step diffs.
fn show_run(n: usize) -> Result<()> {
    let Some(r) = record::nth_recent(n)? else {
        anyhow::bail!("no run #{n} — `stepci runs` lists what's recorded");
    };
    let mark = if r.exit_code == 0 {
        "✓ passed"
    } else {
        "✗ failed"
    };
    println!(
        "● recording of `{}` — {}, {}",
        r.workflow,
        relative_time(r.started_unix_ms),
        mark
    );
    for job in &r.jobs {
        let label = job.name.as_deref().unwrap_or(&job.id);
        println!("\n● job {}{} ({label})", job.id, job.matrix);
        for step in &job.steps {
            render_step(step);
        }
    }
    Ok(())
}

fn render_step(s: &record::StepRecord) {
    let icon = match s.outcome.as_str() {
        "success" => "✓",
        "failure" => "✗",
        _ => "⤼",
    };
    println!("  {icon} step {}: {}", s.number, s.label);

    if s.outcome == "skipped" {
        for line in &s.skip_reason {
            println!("      {line}");
        }
        return;
    }
    if let Some(f) = &s.failure {
        println!("      ↳ {f}");
    }
    if !s.env_added.is_empty()
        || !s.env_changed.is_empty()
        || !s.env_removed.is_empty()
        || !s.path_added.is_empty()
    {
        println!("    env:");
        for (k, v) in &s.env_added {
            println!("      + {k} = {v}");
        }
        for (k, o, n) in &s.env_changed {
            println!("      ~ {k}: {o} → {n}");
        }
        for k in &s.env_removed {
            println!("      - {k}");
        }
        for p in &s.path_added {
            println!("      + PATH ⊕ {p}");
        }
    }
    if !s.files_added.is_empty() || !s.files_removed.is_empty() || !s.files_modified.is_empty() {
        println!("    files:");
        for f in &s.files_added {
            println!("      + {f}");
        }
        for f in &s.files_removed {
            println!("      - {f}");
        }
        for f in &s.files_modified {
            println!("      ~ {f}");
        }
    }
    if s.files_truncated {
        println!("    (filesystem diff was skipped: workspace too large)");
    }
}

/// A coarse "how long ago" for a Unix-ms timestamp.
fn relative_time(started_ms: u128) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let secs = now.saturating_sub(started_ms) / 1000;
    match secs {
        0..=4 => "just now".to_string(),
        5..=59 => format!("{secs}s ago"),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}
