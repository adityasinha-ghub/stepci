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
    /// Compare two recorded runs (by recency; 1 = most recent).
    Diff {
        /// The "before" run (default: 2, the previous run).
        #[arg(default_value = "2")]
        a: usize,
        /// The "after" run (default: 1, the most recent).
        #[arg(default_value = "1")]
        b: usize,
    },
    /// Print a file's recorded content from a run (as of the last step to write it).
    Cat {
        /// Workspace-relative path, as shown by `stepci show`/`diff`.
        path: String,
        /// Which run — 1 is the most recent (the default).
        #[arg(long, default_value = "1")]
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
        Command::Diff { a, b } => diff_runs(a, b),
        Command::Cat { path, run } => cat_file(&path, run),
    }
}

/// `stepci cat <path> [--run n]` — print a file's recorded content from a run.
fn cat_file(path: &str, n: usize) -> Result<()> {
    let Some(r) = record::nth_recent(n)? else {
        anyhow::bail!("no run #{n} — `stepci runs` lists what's recorded");
    };
    match record::file_content(&r, path) {
        Some(bytes) => {
            use std::io::Write;
            std::io::stdout().write_all(&bytes).ok();
        }
        None => anyhow::bail!(
            "`{path}` wasn't captured in run #{n} (not changed by a step, too large, or a directory)"
        ),
    }
    Ok(())
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

/// `stepci diff <a> <b>` — compare two recorded runs (by recency).
fn diff_runs(a_idx: usize, b_idx: usize) -> Result<()> {
    if a_idx == b_idx {
        anyhow::bail!("pick two different runs (got #{a_idx} twice)");
    }
    let runs = record::list()?;
    let get = |n: usize| runs.get(n.saturating_sub(1));
    let (Some(a), Some(b)) = (get(a_idx), get(b_idx)) else {
        anyhow::bail!("not enough recorded runs — `stepci runs` lists what's available");
    };

    println!(
        "Comparing #{a_idx} ({}) → #{b_idx} ({})   [{}]",
        relative_time(a.started_unix_ms),
        relative_time(b.started_unix_ms),
        b.workflow,
    );
    let mut any = false;
    if a.exit_code != b.exit_code {
        any = true;
        println!(
            "  overall: {} → {}",
            outcome_word(a.exit_code),
            outcome_word(b.exit_code)
        );
    }

    // Align jobs by (id + matrix); within a matched job, align steps positionally.
    let key = |j: &record::JobRecord| format!("{}{}", j.id, j.matrix);
    let b_jobs: std::collections::HashMap<String, &record::JobRecord> =
        b.jobs.iter().map(|j| (key(j), j)).collect();
    let a_keys: std::collections::HashSet<String> = a.jobs.iter().map(key).collect();

    for ja in &a.jobs {
        let Some(jb) = b_jobs.get(&key(ja)) else {
            any = true;
            println!("  job {}{}: only in #{a_idx}", ja.id, ja.matrix);
            continue;
        };
        let mut lines: Vec<String> = Vec::new();
        if ja.steps.len() != jb.steps.len() {
            lines.push(format!(
                "    (structure differs: {} steps → {} — comparing positionally)",
                ja.steps.len(),
                jb.steps.len()
            ));
        }
        for i in 0..ja.steps.len().max(jb.steps.len()) {
            match (ja.steps.get(i), jb.steps.get(i)) {
                (Some(sa), Some(sb)) => {
                    let changes = record::step_changes(sa, sb);
                    let content = content_diff_lines(sa, sb);
                    if !changes.is_empty() || !content.is_empty() {
                        let label = if sa.label == sb.label {
                            sa.label.clone()
                        } else {
                            format!("{} / {}", sa.label, sb.label)
                        };
                        lines.push(format!("    step {}: {label}", i + 1));
                        lines.extend(changes.into_iter().map(|c| format!("        {c}")));
                        lines.extend(content);
                    }
                }
                (Some(sa), None) => lines.push(format!(
                    "    step {}: {} — only in #{a_idx}",
                    i + 1,
                    sa.label
                )),
                (None, Some(sb)) => lines.push(format!(
                    "    step {}: {} — only in #{b_idx}",
                    i + 1,
                    sb.label
                )),
                (None, None) => {}
            }
        }
        if !lines.is_empty() {
            any = true;
            println!("  job {}{}:", ja.id, ja.matrix);
            for l in lines {
                println!("{l}");
            }
        }
    }
    for jb in &b.jobs {
        if !a_keys.contains(&key(jb)) {
            any = true;
            println!("  job {}{}: only in #{b_idx}", jb.id, jb.matrix);
        }
    }

    if !any {
        println!("  no differences — the runs match (as far as the recording captured).");
    }
    Ok(())
}

fn outcome_word(exit: i32) -> &'static str {
    if exit == 0 {
        "✓ passed"
    } else {
        "✗ failed"
    }
}

/// Line-level content diffs for files both steps changed to *different* content
/// (using the stored blobs). Text only; capped; binary content is noted, not dumped.
fn content_diff_lines(sa: &record::StepRecord, sb: &record::StepRecord) -> Vec<String> {
    use similar::{ChangeTag, TextDiff};
    const MAX_LINES: usize = 40;

    let after: std::collections::HashMap<&str, &record::FileChange> =
        sb.files.iter().map(|f| (f.path.as_str(), f)).collect();
    let mut out = Vec::new();
    for fa in &sa.files {
        let Some(fb) = after.get(fa.path.as_str()) else {
            continue;
        };
        let (Some(ha), Some(hb)) = (&fa.hash, &fb.hash) else {
            continue;
        };
        if ha == hb {
            continue;
        }
        let (Some(ba), Some(bb)) = (record::load_blob(ha), record::load_blob(hb)) else {
            continue; // blob unavailable (e.g. GC'd) — the summary line already noted it
        };
        match (std::str::from_utf8(&ba), std::str::from_utf8(&bb)) {
            (Ok(ta), Ok(tb)) => {
                out.push(format!("        ─ {} ─", fa.path));
                let mut shown = 0;
                for change in TextDiff::from_lines(ta, tb).iter_all_changes() {
                    let sign = match change.tag() {
                        ChangeTag::Delete => "-",
                        ChangeTag::Insert => "+",
                        ChangeTag::Equal => continue, // only show the changed lines
                    };
                    if shown >= MAX_LINES {
                        out.push("          … (diff truncated)".to_string());
                        break;
                    }
                    out.push(format!(
                        "          {sign} {}",
                        change.value().trim_end_matches('\n')
                    ));
                    shown += 1;
                }
            }
            _ => out.push(format!("        ─ {} ─ (binary content differs)", fa.path)),
        }
    }
    out
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
    if !s.files.is_empty() {
        println!("    files:");
        for f in &s.files {
            let marker = match f.status.as_str() {
                "added" => "+",
                "removed" => "-",
                _ => "~",
            };
            let name = match f.dir_files {
                Some(n) => format!("{}/ ({n} files)", f.path),
                None => f.path.clone(),
            };
            println!("      {marker} {name}");
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
