//! The native executor: run a workflow's jobs and `run:` steps directly on the
//! host — no Docker — evaluating `if:` conditions and interpolating `${{ }}` with
//! [`crate::expr`], and propagating `$GITHUB_ENV`/`$GITHUB_PATH`/`$GITHUB_OUTPUT`
//! between steps like the real runner.
//!
//! `run:` steps and composite, JavaScript, and Docker `uses:` actions run (local
//! and remote); Docker is used only for Docker actions. A step's stdout is
//! streamed through while being scanned for `::workflow-command::` lines (e.g.
//! `set-output`, `add-mask`), matching the runner's second back-channel.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

use anyhow::{Context as _, Result, bail};
use indexmap::IndexMap;
use tempfile::NamedTempFile;

use crate::diff::{self, Entry, EnvDiff, FsDiff};
use crate::envfile;
use crate::expr::{self, Context, JobStatus};
use crate::fetch;
use crate::model::{ActionDef, Conditional, Job, Runs, Step, StepAction, Workflow};
use crate::parse;
use crate::value::Value;
use crate::wfcmd;

/// Cap on files walked per workspace snapshot, so a huge tree can't hang a step.
const MAX_DIFF_FILES: usize = 20_000;

/// Options for a run.
pub struct RunOptions {
    /// Run only this job (and don't auto-run its `needs`). `None` runs all jobs.
    pub job: Option<String>,
    /// The working directory used as `github.workspace` and the base for
    /// relative `working-directory`.
    pub workspace: PathBuf,
    /// Pause before every step (interactive step-through).
    pub step_all: bool,
    /// Pause before steps whose id is in this list.
    pub breakpoints: Vec<String>,
    /// Resolved secrets, exposed to expressions as `secrets.NAME`.
    pub secrets: IndexMap<String, String>,
}

/// What to do after a step: keep going, or the user asked to quit the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    Continue,
    Quit,
}

/// The outcome of a whole job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobResult {
    Success,
    Failed,
    Skipped,
}

/// Run a workflow. Returns the process exit code (0 on success, 1 if any job
/// failed).
pub fn run_workflow(wf: &Workflow, opts: &RunOptions) -> Result<i32> {
    validate_needs(wf)?;
    let github = build_github(&opts.workspace);
    let runner = build_runner();
    let order = job_order(wf, opts.job.as_deref())?;
    // In single-job mode we don't run `needs`, so don't gate on them.
    let single = opts.job.is_some();

    // Pausing needs a real terminal to read commands from.
    let pausing_requested = opts.step_all || !opts.breakpoints.is_empty();
    let interactive = pausing_requested && std::io::stdin().is_terminal();
    if pausing_requested && !interactive {
        eprintln!("note: stdin is not a terminal — running without pausing.");
    }
    warn_unknown_breakpoints(wf, opts);

    let mut quit = false;
    let mut results: IndexMap<String, JobResult> = IndexMap::new();
    'jobs: for id in &order {
        let job = &wf.jobs[id];
        let label = job.name.as_deref().unwrap_or(&job.id);
        // A job's implicit `success()` means every needed job succeeded.
        let needs_ok = single
            || job
                .needs
                .iter()
                .all(|n| matches!(results.get(n), Some(JobResult::Success)));
        let ctx_status = if needs_ok {
            JobStatus::Success
        } else {
            JobStatus::Failure
        };

        // A needed job failed and there's no `if:` to override → skip the whole job.
        if !needs_ok && job.if_cond.is_none() {
            println!("\n● job {id}: skipped (a needed job did not succeed)");
            results.insert(id.clone(), JobResult::Skipped);
            continue;
        }

        // Expand the matrix (a non-matrix job is one empty combination).
        let combos = match job.strategy.as_ref().and_then(|s| s.matrix.as_ref()) {
            Some(m) => expand_matrix(m),
            None => vec![IndexMap::new()],
        };
        let fail_fast = job.strategy.as_ref().map(|s| s.fail_fast).unwrap_or(true);
        if combos.is_empty() {
            println!("\n● job {id}: skipped (matrix produced no combinations)");
            results.insert(id.clone(), JobResult::Skipped);
            continue;
        }

        let mut job_result = JobResult::Skipped; // upgraded once a combo runs
        for combo in &combos {
            let suffix = if combo.is_empty() {
                String::new()
            } else {
                format!(
                    " [{}]",
                    combo.values().cloned().collect::<Vec<_>>().join(", ")
                )
            };
            let matrix_ctx: IndexMap<String, Value> = combo
                .iter()
                .map(|(k, v)| (k.clone(), Value::Str(v.clone())))
                .collect();

            let should_run = match &job.if_cond {
                Some(cond) => {
                    let jctx = make_job_context(
                        wf,
                        &results,
                        job,
                        &github,
                        &runner,
                        &opts.secrets,
                        &matrix_ctx,
                        ctx_status,
                    );
                    eval_condition(cond, &jctx)?
                }
                None => needs_ok,
            };
            if !should_run {
                println!("\n● job {id}{suffix}: skipped (if condition false)");
                continue;
            }

            println!("\n● job {id}{suffix} ({label})");
            let (status, flow) =
                run_job(job, wf, &github, &runner, opts, interactive, &matrix_ctx)?;
            if status == JobStatus::Failure {
                job_result = JobResult::Failed;
            } else if job_result == JobResult::Skipped {
                job_result = JobResult::Success;
            }
            if flow == Flow::Quit {
                results.insert(id.clone(), job_result);
                println!("\n(quit — remaining jobs not run)");
                quit = true;
                break 'jobs;
            }
            if status == JobStatus::Failure && fail_fast && combos.len() > 1 {
                println!("  (fail-fast: skipping the remaining matrix combinations)");
                break;
            }
        }
        results.insert(id.clone(), job_result);
    }

    // A real failure outranks a user quit; a clean quit is reported as 130
    // (interrupted) so it isn't mistaken for success.
    let failed = results.values().any(|r| *r == JobResult::Failed);
    Ok(if failed {
        1
    } else if quit {
        130
    } else {
        0
    })
}

/// Warn once about `--break` ids that match no step, so a typo isn't silent.
fn warn_unknown_breakpoints(wf: &Workflow, opts: &RunOptions) {
    if opts.breakpoints.is_empty() {
        return;
    }
    let known: HashSet<&str> = wf
        .jobs
        .values()
        .flat_map(|j| j.steps.iter())
        .filter_map(|s| s.id.as_deref())
        .collect();
    for bp in &opts.breakpoints {
        if !known.contains(bp.as_str()) {
            eprintln!("note: no step has id `{bp}` — that breakpoint will never fire.");
        }
    }
}

/// Expand a `strategy.matrix` into concrete combinations: the cartesian product
/// of its dimensions, then `exclude` removals, then `include` merges/additions.
fn expand_matrix(m: &crate::model::Matrix) -> Vec<IndexMap<String, String>> {
    let mut combos: Vec<IndexMap<String, String>> = vec![IndexMap::new()];
    for (dim, values) in &m.dimensions {
        let mut next = Vec::new();
        for combo in &combos {
            for v in values {
                let mut c = combo.clone();
                c.insert(dim.clone(), v.clone());
                next.push(c);
            }
        }
        combos = next;
    }
    if m.dimensions.is_empty() {
        combos.clear(); // no base product; only `include` entries become combos
    }

    combos.retain(|combo| {
        !m.exclude
            .iter()
            .any(|ex| ex.iter().all(|(k, v)| combo.get(k) == Some(v)))
    });

    // Each `include` merges into the base combinations whose original-dimension
    // values it matches (adding/overwriting only non-dimension keys), and if it
    // matches none it becomes a standalone combination. Matching considers only
    // the base product — never combos an earlier `include` appended — matching
    // GitHub, so several includes that each add a new value stay distinct.
    let mut extra: Vec<IndexMap<String, String>> = Vec::new();
    for inc in &m.include {
        let dim_keys: Vec<&String> = inc
            .keys()
            .filter(|k| m.dimensions.contains_key(*k))
            .collect();
        let mut merged = false;
        for combo in combos.iter_mut() {
            if dim_keys.iter().all(|k| combo.get(*k) == inc.get(*k)) {
                for (k, v) in inc {
                    combo.insert(k.clone(), v.clone());
                }
                merged = true;
            }
        }
        if !merged {
            extra.push(inc.clone());
        }
    }
    combos.extend(extra);
    combos
}

/// Reject a `needs` that references a job the workflow doesn't define — GitHub
/// rejects these, and silently ignoring them would run jobs out of order.
fn validate_needs(wf: &Workflow) -> Result<()> {
    for (id, job) in &wf.jobs {
        for n in &job.needs {
            if !wf.jobs.contains_key(n) {
                bail!("job `{id}` needs `{n}`, which is not a defined job");
            }
        }
    }
    Ok(())
}

/// Build the evaluation context for a job-level `if:` — `needs.<job>.result`,
/// plus github/runner/env and the needs-derived status.
#[allow(clippy::too_many_arguments)]
fn make_job_context(
    wf: &Workflow,
    results: &IndexMap<String, JobResult>,
    job: &Job,
    github: &Value,
    runner: &Value,
    secrets: &IndexMap<String, String>,
    matrix: &IndexMap<String, Value>,
    status: JobStatus,
) -> Context {
    let mut needs = IndexMap::new();
    for n in &job.needs {
        let result = match results.get(n) {
            Some(JobResult::Success) => "success",
            Some(JobResult::Failed) => "failure",
            _ => "skipped",
        };
        let mut obj = IndexMap::new();
        obj.insert("result".to_string(), Value::Str(result.to_string()));
        obj.insert("outputs".to_string(), Value::Object(IndexMap::new()));
        needs.insert(n.clone(), Value::Object(obj));
    }

    let mut job_obj = IndexMap::new();
    job_obj.insert(
        "status".to_string(),
        Value::Str(status_str(status).to_string()),
    );

    let mut vars = IndexMap::new();
    vars.insert("github".to_string(), github.clone());
    vars.insert("runner".to_string(), runner.clone());
    vars.insert("env".to_string(), str_object(&wf.env));
    vars.insert("needs".to_string(), Value::Object(needs));
    vars.insert("job".to_string(), Value::Object(job_obj));
    vars.insert("secrets".to_string(), str_object(secrets));
    vars.insert("matrix".to_string(), Value::Object(matrix.clone()));
    Context { vars, status }
}

/// Accumulated state as a job's steps run.
struct JobState {
    /// Workflow+job+`GITHUB_ENV` overrides, layered in order; the `env` context.
    env: IndexMap<String, String>,
    /// `GITHUB_PATH` additions, prepended to `PATH` (most-recent first).
    path: Vec<String>,
    /// The `steps` context: id → `{ outputs, outcome, conclusion }`.
    steps: IndexMap<String, Value>,
    /// The `matrix` context for this job run (empty for non-matrix jobs).
    matrix: IndexMap<String, Value>,
    /// Running job status, which drives `if:` and the status functions.
    status: JobStatus,
}

fn run_job(
    job: &Job,
    wf: &Workflow,
    github: &Value,
    runner: &Value,
    opts: &RunOptions,
    interactive: bool,
    matrix: &IndexMap<String, Value>,
) -> Result<(JobStatus, Flow)> {
    let mut state = JobState {
        env: IndexMap::new(),
        path: Vec::new(),
        steps: IndexMap::new(),
        matrix: matrix.clone(),
        status: JobStatus::Success,
    };

    // Layer workflow then job env, interpolating each value as it's added so it
    // can reference earlier entries and the github/runner contexts.
    layer_env(&mut state, &wf.env, github, runner, &opts.secrets)?;
    layer_env(&mut state, &job.env, github, runner, &opts.secrets)?;

    for (i, step) in job.steps.iter().enumerate() {
        let flow = run_step(
            step,
            i + 1,
            &mut state,
            job,
            wf,
            github,
            runner,
            opts,
            interactive,
        )
        .with_context(|| format!("in step {}", i + 1))?;
        if flow == Flow::Quit {
            return Ok((state.status, Flow::Quit));
        }
    }

    Ok((state.status, Flow::Continue))
}

#[allow(clippy::too_many_arguments)]
fn run_step(
    step: &Step,
    number: usize,
    state: &mut JobState,
    job: &Job,
    wf: &Workflow,
    github: &Value,
    runner: &Value,
    opts: &RunOptions,
    interactive: bool,
) -> Result<Flow> {
    // Step env is layered over the job env for this step only.
    let mut step_env = state.env.clone();
    let mut ctx0 = make_context(
        &step_env,
        github,
        runner,
        &state.steps,
        &opts.secrets,
        state.status,
    );
    ctx0.vars
        .insert("matrix".to_string(), Value::Object(state.matrix.clone()));
    for (k, v) in &step.env {
        step_env.insert(k.clone(), expr::interpolate(v, &ctx0)?);
    }
    let mut ctx = make_context(
        &step_env,
        github,
        runner,
        &state.steps,
        &opts.secrets,
        state.status,
    );
    ctx.vars
        .insert("matrix".to_string(), Value::Object(state.matrix.clone()));

    let label = step_label(step);
    print!("  ▸ step {number}: {label} … ");
    let _ = std::io::stdout().flush();

    // Decide whether to run.
    let should_run = match &step.if_cond {
        Some(cond) => eval_condition(cond, &ctx)?,
        None => state.status == JobStatus::Success,
    };
    if !should_run {
        println!("skipped (if condition false)");
        record_step(state, step, "skipped", "skipped", IndexMap::new());
        return Ok(Flow::Continue);
    }

    let script = match &step.action {
        StepAction::Run { script } => expr::interpolate(script, &ctx)?,
        StepAction::Uses { action, with } => {
            return run_uses_step(
                step, number, state, &step_env, github, runner, opts, action, with, &ctx,
            );
        }
    };

    // Resolve shell + working directory.
    let shell = step
        .shell
        .as_deref()
        .or(job.defaults.shell.as_deref())
        .or(wf.defaults.shell.as_deref())
        .unwrap_or("bash");
    let cwd = resolve_cwd(step, job, wf, &opts.workspace);

    println!(); // end the "… " header line

    // Interactive pause before running this step.
    if interactive && should_pause_at(step, opts) {
        match pause_before(
            number,
            &label,
            shell,
            &script,
            &cwd,
            &step_env,
            &state.path,
            github,
            runner,
            &opts.secrets,
        )? {
            Decision::Run => {}
            Decision::Skip => {
                println!("  ⤼ step {number} skipped by request");
                record_step(state, step, "skipped", "skipped", IndexMap::new());
                return Ok(Flow::Continue);
            }
            Decision::Quit => return Ok(Flow::Quit),
        }
    }

    // Snapshot before running, so we can diff what the step changed.
    let env_before = state.env.clone();
    let path_before = state.path.clone();
    let fs_before = diff::snapshot_fs(&opts.workspace, MAX_DIFF_FILES);

    let io = execute_script(
        shell,
        &script,
        &cwd,
        &step_env,
        &state.path,
        github,
        runner,
        &opts.secrets,
    )?;

    // Apply everything the step exported, regardless of exit status.
    for (k, v) in &io.env_additions {
        state.env.insert(k.clone(), v.clone());
    }
    for p in io.path_additions {
        state.path.insert(0, p); // most recently added wins
    }
    let outputs = io.outputs;

    // Diff env + filesystem against the pre-run snapshot.
    let fs_after = diff::snapshot_fs(&opts.workspace, MAX_DIFF_FILES);
    let env_delta = diff::env_diff(&env_before, &state.env, &path_before, &state.path);
    let fs_delta = diff::fs_diff(&fs_before, &fs_after);

    let ok = io.status.success();
    let (outcome, conclusion) = if ok {
        ("success", "success")
    } else if step_continues_on_error(step, &ctx)? {
        ("failure", "success") // failed, but the job carries on
    } else {
        ("failure", "failure")
    };
    if conclusion == "failure" {
        state.status = JobStatus::Failure;
    }
    record_step(
        state,
        step,
        outcome,
        conclusion,
        outputs.into_iter().collect(),
    );

    let code = io.status.code().unwrap_or(-1);
    match (ok, conclusion) {
        (true, _) => println!("  ✓ step {number} ok"),
        (false, "success") => {
            println!("  ⚠ step {number} failed (exit {code}) — continue-on-error")
        }
        (false, _) => println!("  ✗ step {number} failed (exit {code})"),
    }
    print_diff(&env_delta, &fs_delta, &opts.secrets);
    Ok(Flow::Continue)
}

// ---------------------------------------------------------------------------
// `uses:` actions (M7: local composite actions run natively)
// ---------------------------------------------------------------------------

/// What a `uses:` reference resolves to.
enum ResolvedAction {
    /// A composite action: its definition and the directory it lives in.
    Composite(Box<ActionDef>, PathBuf),
    /// A JavaScript action: its definition and directory.
    Node(Box<ActionDef>, PathBuf),
    /// A Docker action: its definition and directory (build context).
    Docker(Box<ActionDef>, PathBuf),
    /// Recognized but not runnable yet — carries a reason to show the user.
    Unsupported(String),
}

/// Resolve a `uses:` reference to a runnable action. Local `./…` and remote
/// `owner/repo@ref` composite/JS/Docker actions run; a bare `docker://image`
/// runs that image directly.
fn resolve_action(reference: &str, workspace: &Path) -> Result<ResolvedAction> {
    if let Some(image) = reference.strip_prefix("docker://") {
        // A direct image reference: a Docker action with no metadata.
        let def = ActionDef {
            inputs: IndexMap::new(),
            outputs: IndexMap::new(),
            runs: Runs::Docker {
                image: image.to_string(),
                entrypoint: None,
                args: Vec::new(),
                env: IndexMap::new(),
            },
        };
        return Ok(ResolvedAction::Docker(
            Box::new(def),
            workspace.to_path_buf(),
        ));
    }

    let dir = if reference.starts_with("./") || reference.starts_with("../") {
        workspace.join(reference)
    } else if let Some(remote) = fetch::parse_remote(reference) {
        println!("  ⟳ fetching {reference}");
        fetch::fetch(&remote, &fetch::cache_root()?)?
    } else {
        return Ok(ResolvedAction::Unsupported(format!(
            "unrecognized `uses:` reference `{reference}`"
        )));
    };

    let action_file = ["action.yml", "action.yaml"]
        .iter()
        .map(|f| dir.join(f))
        .find(|p| p.exists())
        .ok_or_else(|| anyhow::anyhow!("no action.yml/action.yaml in `{}`", dir.display()))?;

    let def = parse::parse_action_file(&action_file)?;
    match &def.runs {
        Runs::Composite { .. } => Ok(ResolvedAction::Composite(Box::new(def), dir)),
        Runs::Node { .. } => Ok(ResolvedAction::Node(Box::new(def), dir)),
        Runs::Docker { .. } => Ok(ResolvedAction::Docker(Box::new(def), dir)),
    }
}

/// Handle a `uses:` step: run it (composite) or report why it was skipped.
#[allow(clippy::too_many_arguments)]
fn run_uses_step(
    step: &Step,
    number: usize,
    state: &mut JobState,
    step_env: &IndexMap<String, String>,
    github: &Value,
    runner: &Value,
    opts: &RunOptions,
    reference: &str,
    with: &IndexMap<String, String>,
    ctx: &Context,
) -> Result<Flow> {
    println!(); // end the "… " header line

    #[derive(Clone, Copy)]
    enum Kind {
        Composite,
        Node,
        Docker,
    }
    let resolved = resolve_action(reference, &opts.workspace)?;
    let (def, dir, kind) = match resolved {
        ResolvedAction::Composite(def, dir) => (def, dir, Kind::Composite),
        ResolvedAction::Node(def, dir) => (def, dir, Kind::Node),
        ResolvedAction::Docker(def, dir) => (def, dir, Kind::Docker),
        ResolvedAction::Unsupported(reason) => {
            println!("  ⤼ step {number} skipped ({reason})");
            record_step(state, step, "skipped", "skipped", IndexMap::new());
            return Ok(Flow::Continue);
        }
    };

    // Resolve the inputs the action sees: `with:` values over declared defaults.
    let inputs = resolve_inputs(&def, with, ctx)?;

    let env_before = state.env.clone();
    let path_before = state.path.clone();
    let fs_before = diff::snapshot_fs(&opts.workspace, MAX_DIFF_FILES);

    let kind_label = match kind {
        Kind::Composite => "composite",
        Kind::Node => "javascript",
        Kind::Docker => "docker",
    };
    println!("  ┌ {kind_label} action `{reference}`");
    let (outputs, ok) = match kind {
        Kind::Composite => run_composite(&def, &inputs, state, github, runner, opts, &dir)?,
        Kind::Node => {
            let Runs::Node { main, .. } = &def.runs else {
                unreachable!("resolved as Node")
            };
            run_node_action(main, &dir, &inputs, step_env, state, github, runner, opts)?
        }
        Kind::Docker => {
            run_docker_action(&def, &dir, &inputs, step_env, state, github, runner, opts)?
        }
    };
    println!("  └ end `{reference}`");

    let fs_after = diff::snapshot_fs(&opts.workspace, MAX_DIFF_FILES);
    let env_delta = diff::env_diff(&env_before, &state.env, &path_before, &state.path);
    let fs_delta = diff::fs_diff(&fs_before, &fs_after);

    let (outcome, conclusion) = if ok {
        ("success", "success")
    } else if step_continues_on_error(step, ctx)? {
        ("failure", "success")
    } else {
        ("failure", "failure")
    };
    if conclusion == "failure" {
        state.status = JobStatus::Failure;
    }
    record_step(state, step, outcome, conclusion, outputs);

    match (ok, conclusion) {
        (true, _) => println!("  ✓ step {number} ok"),
        (false, "success") => println!("  ⚠ step {number} failed — continue-on-error"),
        (false, _) => println!("  ✗ step {number} failed"),
    }
    print_diff(&env_delta, &fs_delta, &opts.secrets);
    Ok(Flow::Continue)
}

/// Run a JavaScript action: `node <dir>/<main>` with `INPUT_*` + the standard
/// runner env and channel files, then read back its env/path/outputs. Uses the
/// host's `node` (a later milestone can pin the version).
#[allow(clippy::too_many_arguments)]
fn run_node_action(
    main: &str,
    action_dir: &Path,
    inputs: &IndexMap<String, String>,
    base_env: &IndexMap<String, String>,
    state: &mut JobState,
    github: &Value,
    runner: &Value,
    opts: &RunOptions,
) -> Result<(IndexMap<String, String>, bool)> {
    let entry = action_dir.join(main);
    if !entry.exists() {
        bail!("action entry point `{}` not found", entry.display());
    }

    let env_file = NamedTempFile::new().context("creating GITHUB_ENV file")?;
    let out_file = NamedTempFile::new().context("creating GITHUB_OUTPUT file")?;
    let path_file = NamedTempFile::new().context("creating GITHUB_PATH file")?;
    let state_file = NamedTempFile::new().context("creating GITHUB_STATE file")?;
    let summary_file = NamedTempFile::new().context("creating GITHUB_STEP_SUMMARY file")?;

    // The action's environment: the job+step env, then this action's `INPUT_*`.
    let mut step_env = base_env.clone();
    for (k, v) in input_env_vars(inputs) {
        step_env.insert(k, v);
    }

    let mut cmd = Command::new("node");
    cmd.arg(&entry).current_dir(&opts.workspace);
    apply_common_env(
        &mut cmd,
        &step_env,
        &state.path,
        github,
        runner,
        &opts.workspace,
    );
    cmd.env("GITHUB_ENV", env_file.path());
    cmd.env("GITHUB_OUTPUT", out_file.path());
    cmd.env("GITHUB_PATH", path_file.path());
    cmd.env("GITHUB_STATE", state_file.path());
    cmd.env("GITHUB_STEP_SUMMARY", summary_file.path());
    cmd.env("GITHUB_ACTION_PATH", action_dir);

    let (status, scan) = run_capturing(cmd, &opts.secrets)
        .map_err(|e| anyhow::anyhow!("running `node` (is Node.js installed?): {e:#}"))?;

    for (k, v) in read_key_values(env_file.path())? {
        state.env.insert(k, v);
    }
    for p in read_path_additions(path_file.path())? {
        state.path.insert(0, p);
    }
    let mut outputs: IndexMap<String, String> =
        read_key_values(out_file.path())?.into_iter().collect();
    for (k, v) in scan.outputs {
        outputs.insert(k, v);
    }
    Ok((outputs, status.success()))
}

/// Run a Docker action: build (if a `Dockerfile`) or use a prebuilt image, then
/// `docker run` it with the workspace mounted at `/github/workspace`, `INPUT_*`
/// env, and the channel files mounted so its `$GITHUB_*` writes come back.
///
/// This is the one place we shell out to Docker (the "Docker only when required"
/// stance). Containers run as root, so files they create in the workspace are
/// root-owned on the host.
#[allow(clippy::too_many_arguments)]
fn run_docker_action(
    def: &ActionDef,
    action_dir: &Path,
    inputs: &IndexMap<String, String>,
    base_env: &IndexMap<String, String>,
    state: &mut JobState,
    github: &Value,
    runner: &Value,
    opts: &RunOptions,
) -> Result<(IndexMap<String, String>, bool)> {
    let Runs::Docker {
        image,
        entrypoint,
        args,
        env: docker_env,
    } = &def.runs
    else {
        return Ok((IndexMap::new(), true));
    };

    // Resolve the image. `docker://name` is a prebuilt registry image; otherwise
    // a value naming a file that exists under the action (e.g. `Dockerfile`,
    // `./Dockerfile`, `build/Containerfile`) is built; anything else is treated
    // as a prebuilt image name.
    let image_name = if let Some(img) = image.strip_prefix("docker://") {
        img.to_string()
    } else if action_dir.join(image).is_file() {
        let dockerfile = action_dir.join(image);
        let tag = docker_tag(action_dir);
        println!("  │ building image from {}", dockerfile.display());
        let built = docker_run(&[
            "build".into(),
            "-q".into(),
            "-t".into(),
            tag.clone(),
            "-f".into(),
            path_str(&dockerfile)?,
            path_str(action_dir)?,
        ])?;
        if !built.success() {
            bail!("docker build failed for `{}`", dockerfile.display());
        }
        tag
    } else {
        image.clone()
    };

    // A host dir for the file-command channels, mounted into the container.
    let chan = tempfile::tempdir().context("creating docker channel dir")?;
    for f in ["env", "output", "path", "state", "summary"] {
        std::fs::File::create(chan.path().join(f)).context("creating docker channel file")?;
    }
    let ws = std::fs::canonicalize(&opts.workspace).unwrap_or_else(|_| opts.workspace.clone());

    let mut run: Vec<String> = vec![
        "run".into(),
        "--rm".into(),
        "-w".into(),
        "/github/workspace".into(),
        "-v".into(),
        format!("{}:/github/workspace", path_str(&ws)?),
        "-v".into(),
        format!("{}:/github/file_commands", path_str(chan.path())?),
    ];

    // Env: INPUT_*, the action's own env, and the reserved runner variables.
    let ictx = make_context_with_inputs(
        base_env,
        github,
        runner,
        &IndexMap::new(),
        &opts.secrets,
        inputs,
        JobStatus::Success,
    );
    // The container's environment: the job+step env, then `INPUT_*`, then the
    // action's own `runs.env` (as on GitHub, which passes job/workflow env into
    // container actions too).
    let mut env = base_env.clone();
    for (k, v) in input_env_vars(inputs) {
        env.insert(k, v);
    }
    for (k, v) in docker_env {
        env.insert(k.clone(), expr::interpolate(v, &ictx)?);
    }
    env.insert("GITHUB_WORKSPACE".into(), "/github/workspace".into());
    env.insert("CI".into(), "true".into());
    env.insert("GITHUB_ACTIONS".into(), "true".into());
    env.insert("RUNNER_OS".into(), "Linux".into());
    for f in ["env", "output", "path", "state", "summary"] {
        env.insert(
            format!("GITHUB_{}", f.to_ascii_uppercase()),
            format!("/github/file_commands/{f}"),
        );
    }
    for (var, key) in [
        ("GITHUB_SHA", "sha"),
        ("GITHUB_REF", "ref"),
        ("GITHUB_REF_NAME", "ref_name"),
        ("GITHUB_EVENT_NAME", "event_name"),
    ] {
        if let Some(val) = object_get(github, key) {
            env.insert(var.to_string(), val);
        }
    }
    for (k, v) in &env {
        run.push("-e".into());
        run.push(format!("{k}={v}"));
    }

    if let Some(ep) = entrypoint {
        run.push("--entrypoint".into());
        run.push(ep.clone());
    }
    run.push(image_name);
    for a in args {
        run.push(expr::interpolate(a, &ictx)?);
    }

    // Capture the container's stdout for workflow commands, like a native step.
    let mut dcmd = Command::new("docker");
    dcmd.args(&run);
    let (status, scan) = run_capturing(dcmd, &opts.secrets)
        .map_err(|e| anyhow::anyhow!("running docker (is it installed and running?): {e:#}"))?;

    // Read the mounted channel files back.
    let read = |name: &str| std::fs::read_to_string(chan.path().join(name)).unwrap_or_default();
    for (k, v) in envfile::parse_key_values(&read("env"))? {
        state.env.insert(k, v);
    }
    for p in envfile::parse_path_additions(&read("path")) {
        state.path.insert(0, p);
    }
    let mut outputs: IndexMap<String, String> = envfile::parse_key_values(&read("output"))?
        .into_iter()
        .collect();
    for (k, v) in scan.outputs {
        outputs.insert(k, v);
    }
    Ok((outputs, status.success()))
}

/// Run `docker` with the given args (stdio inherited), returning its exit status.
fn docker_run(args: &[String]) -> Result<std::process::ExitStatus> {
    Command::new("docker")
        .args(args)
        .status()
        .map_err(|e| anyhow::anyhow!("failed to run docker (is it installed and running?): {e}"))
}

/// A stable image tag for a built action, derived from its directory.
fn docker_tag(dir: &Path) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    dir.hash(&mut h);
    format!("stepci-action:{:x}", h.finish())
}

fn path_str(p: &Path) -> Result<String> {
    p.to_str()
        .map(String::from)
        .ok_or_else(|| anyhow::anyhow!("path is not valid UTF-8: `{}`", p.display()))
}

/// Compute the inputs a composite action sees: each declared input takes its
/// `with:` value if present (interpolated), else its default (interpolated).
fn resolve_inputs(
    def: &ActionDef,
    with: &IndexMap<String, String>,
    ctx: &Context,
) -> Result<IndexMap<String, String>> {
    let mut inputs = IndexMap::new();
    for (name, input) in &def.inputs {
        let value = if let Some(raw) = with.get(name) {
            expr::interpolate(raw, ctx)?
        } else if let Some(default) = &input.default {
            // A default may reference earlier inputs, so evaluate it against a
            // context carrying the inputs resolved so far.
            let mut dctx = ctx.clone();
            dctx.vars.insert("inputs".to_string(), str_object(&inputs));
            expr::interpolate(default, &dctx)?
        } else {
            if input.required {
                bail!("action input `{name}` is required but was not provided");
            }
            String::new()
        };
        inputs.insert(name.clone(), value);
    }
    // Also pass through any `with:` keys the action didn't declare (GitHub does).
    for (name, raw) in with {
        if !inputs.contains_key(name) {
            inputs.insert(name.clone(), expr::interpolate(raw, ctx)?);
        }
    }
    Ok(inputs)
}

/// The `INPUT_<NAME>` environment variables a composite step sees (name
/// uppercased, spaces → `_`), matching the runner.
fn input_env_vars(inputs: &IndexMap<String, String>) -> IndexMap<String, String> {
    inputs
        .iter()
        .map(|(k, v)| {
            (
                format!("INPUT_{}", k.to_ascii_uppercase().replace(' ', "_")),
                v.clone(),
            )
        })
        .collect()
}

/// Run a composite action's steps natively. Returns its outputs and whether it
/// succeeded. `$GITHUB_ENV`/`$GITHUB_PATH` writes propagate to the job (as on
/// GitHub); `$GITHUB_OUTPUT` writes are scoped to the composite's own steps.
fn run_composite(
    def: &ActionDef,
    inputs: &IndexMap<String, String>,
    state: &mut JobState,
    github: &Value,
    runner: &Value,
    opts: &RunOptions,
    dir: &Path,
) -> Result<(IndexMap<String, String>, bool)> {
    let Runs::Composite { steps } = &def.runs else {
        return Ok((IndexMap::new(), true));
    };
    let input_env = input_env_vars(inputs);
    let mut comp_steps: IndexMap<String, Value> = IndexMap::new();
    let mut status = JobStatus::Success;

    for (i, cstep) in steps.iter().enumerate() {
        let ctx = make_context_with_inputs(
            &state.env,
            github,
            runner,
            &comp_steps,
            &opts.secrets,
            inputs,
            status,
        );

        let should_run = match &cstep.if_cond {
            Some(cond) => eval_condition(cond, &ctx)?,
            None => status == JobStatus::Success,
        };
        let label = step_label(cstep);
        print!("  │ ▹ {}: {label} … ", i + 1);
        let _ = std::io::stdout().flush();

        if !should_run {
            println!("skipped (if)");
            record_composite_step(&mut comp_steps, cstep, "skipped", "skipped", Vec::new());
            continue;
        }

        let script = match &cstep.action {
            StepAction::Run { script } => expr::interpolate(script, &ctx)?,
            StepAction::Uses { action, .. } => {
                println!("skipped (nested `uses: {action}` in composite not supported yet)");
                record_composite_step(&mut comp_steps, cstep, "skipped", "skipped", Vec::new());
                continue;
            }
        };

        // Composite `run:` steps require a shell; default to bash on the host.
        let shell = cstep.shell.as_deref().unwrap_or("bash");
        let cwd = resolve_cwd_in(cstep, dir, &opts.workspace);

        let mut step_env = state.env.clone();
        for (k, v) in &input_env {
            step_env.insert(k.clone(), v.clone());
        }
        for (k, v) in &cstep.env {
            step_env.insert(k.clone(), expr::interpolate(v, &ctx)?);
        }

        println!();
        let io = execute_script(
            shell,
            &script,
            &cwd,
            &step_env,
            &state.path,
            github,
            runner,
            &opts.secrets,
        )?;
        for (k, v) in &io.env_additions {
            state.env.insert(k.clone(), v.clone());
        }
        for p in io.path_additions {
            state.path.insert(0, p);
        }

        let ok = io.status.success();
        let conclusion = if ok || step_continues_on_error(cstep, &ctx)? {
            "success"
        } else {
            "failure"
        };
        if conclusion == "failure" {
            status = JobStatus::Failure;
        }
        record_composite_step(
            &mut comp_steps,
            cstep,
            if ok { "success" } else { "failure" },
            conclusion,
            io.outputs,
        );
        let code = io.status.code().unwrap_or(-1);
        match (ok, conclusion) {
            (true, _) => println!("  │   ✓"),
            (false, "success") => println!("  │   ⚠ failed (exit {code}) — continue-on-error"),
            (false, _) => println!("  │   ✗ failed (exit {code})"),
        }
    }

    // Evaluate the action's declared outputs against the final composite context.
    let out_ctx = make_context_with_inputs(
        &state.env,
        github,
        runner,
        &comp_steps,
        &opts.secrets,
        inputs,
        status,
    );
    let mut outputs = IndexMap::new();
    for (name, out) in &def.outputs {
        if let Some(expr_str) = &out.value {
            outputs.insert(name.clone(), expr::interpolate(expr_str, &out_ctx)?);
        }
    }
    Ok((outputs, status == JobStatus::Success))
}

/// Record a composite step's outcome/outputs into the composite-local `steps`
/// context (only steps with an `id` are addressable).
fn record_composite_step(
    steps: &mut IndexMap<String, Value>,
    step: &Step,
    outcome: &str,
    conclusion: &str,
    outputs: Vec<(String, String)>,
) {
    let Some(id) = &step.id else { return };
    let outputs_obj: IndexMap<String, Value> = outputs
        .into_iter()
        .map(|(k, v)| (k, Value::Str(v)))
        .collect();
    let mut obj = IndexMap::new();
    obj.insert("outputs".to_string(), Value::Object(outputs_obj));
    obj.insert("outcome".to_string(), Value::Str(outcome.to_string()));
    obj.insert("conclusion".to_string(), Value::Str(conclusion.to_string()));
    steps.insert(id.clone(), Value::Object(obj));
}

/// Resolve a composite step's working directory: its `working-directory` if set
/// (relative to the workspace), else the workspace.
fn resolve_cwd_in(step: &Step, _action_dir: &Path, workspace: &Path) -> PathBuf {
    match step.working_directory.as_deref() {
        Some(dir) => {
            let p = Path::new(dir);
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                workspace.join(p)
            }
        }
        None => workspace.to_path_buf(),
    }
}

/// Replace any secret value (of a meaningful length) appearing in `s` with `***`,
/// so secrets that interpolation baked into env/scripts don't leak in our output.
///
/// Longest values are masked first, so a secret that is a substring of another
/// can't leave the longer one only partially masked. Note: a step's own
/// stdout/stderr streams directly and is NOT masked.
fn mask_secrets(s: &str, secrets: &IndexMap<String, String>) -> String {
    let mut values: Vec<&str> = secrets
        .values()
        .map(String::as_str)
        .filter(|v| v.len() >= 4)
        .collect();
    values.sort_by_key(|v| std::cmp::Reverse(v.len()));

    let mut out = s.to_string();
    for v in values {
        out = out.replace(v, "***");
    }
    out
}

/// Whether to pause before this step, given the run's flags.
fn should_pause_at(step: &Step, opts: &RunOptions) -> bool {
    opts.step_all
        || step
            .id
            .as_ref()
            .is_some_and(|id| opts.breakpoints.iter().any(|b| b == id))
}

/// What the user chose at a pause prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    Run,
    Skip,
    Quit,
}

/// Interactive pause before a step: show it, and let the user inspect, drop into
/// a shell with the step's exact environment, then continue / skip / quit.
#[allow(clippy::too_many_arguments)]
fn pause_before(
    number: usize,
    label: &str,
    shell: &str,
    script: &str,
    cwd: &Path,
    step_env: &IndexMap<String, String>,
    path: &[String],
    github: &Value,
    runner: &Value,
    secrets: &IndexMap<String, String>,
) -> Result<Decision> {
    println!("  ⏸  paused before step {number}: {label}");
    println!("     shell: {shell}   cwd: {}", cwd.display());
    loop {
        print!("     [c]ontinue  [s]hell  [i]nfo  s[k]ip  [q]uit > ");
        std::io::stdout().flush().ok();

        let mut line = String::new();
        let n = std::io::stdin().lock().read_line(&mut line)?;
        if n == 0 {
            // EOF (Ctrl-D): treat as quit, since we can't prompt any further.
            println!();
            return Ok(Decision::Quit);
        }
        let cmd = line.trim().to_ascii_lowercase();
        match cmd.as_str() {
            "" | "c" | "continue" => return Ok(Decision::Run),
            "k" | "skip" => return Ok(Decision::Skip),
            "q" | "quit" => return Ok(Decision::Quit),
            "i" | "info" => print_step_info(script, step_env, secrets),
            // A shell that fails to launch must not tear down the run — report
            // and re-prompt, so the debugger stays alive.
            "s" | "shell" => {
                if let Err(e) = drop_into_shell(cwd, step_env, path, github, runner) {
                    println!("     shell error: {e:#}");
                }
            }
            other => println!("     unknown command `{other}` (c/s/i/k/q)"),
        }
    }
}

/// Print the resolved script and the step's environment overrides, masking any
/// secret values that interpolation baked in.
fn print_step_info(
    script: &str,
    step_env: &IndexMap<String, String>,
    secrets: &IndexMap<String, String>,
) {
    println!("     ── script ──");
    for l in script.lines() {
        println!("     {}", mask_secrets(l, secrets));
    }
    if !step_env.is_empty() {
        println!("     ── env ──");
        for (k, v) in step_env {
            println!("     {k}={}", clip(&mask_secrets(v, secrets)));
        }
    }
}

/// Drop into an interactive `$SHELL` with the step's environment and cwd.
fn drop_into_shell(
    cwd: &Path,
    step_env: &IndexMap<String, String>,
    path: &[String],
    github: &Value,
    runner: &Value,
) -> Result<()> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "bash".to_string());
    println!("     ↪ {shell} — the step's env & cwd; type `exit` to return to stepci");
    println!("       (note: $GITHUB_ENV/$GITHUB_OUTPUT writes here do not round-trip)\n");
    let mut cmd = Command::new(&shell);
    cmd.current_dir(cwd);
    apply_common_env(&mut cmd, step_env, path, github, runner, cwd);
    cmd.status()
        .map_err(|e| anyhow::anyhow!("failed to launch shell `{shell}`: {e}"))?;
    println!();
    Ok(())
}

/// Print the per-step env + filesystem diff (the wedge), omitting empty sections
/// and masking any secret values.
fn print_diff(env: &EnvDiff, fs: &FsDiff, secrets: &IndexMap<String, String>) {
    if !env.is_empty() {
        println!("    env:");
        for (k, v) in &env.added {
            println!("      + {k} = {}", clip(&mask_secrets(v, secrets)));
        }
        for (k, old, new) in &env.changed {
            println!(
                "      ~ {k}: {} → {}",
                clip(&mask_secrets(old, secrets)),
                clip(&mask_secrets(new, secrets))
            );
        }
        for k in &env.removed {
            println!("      - {k}");
        }
        for p in &env.path_added {
            println!("      + PATH ⊕ {p}");
        }
    }
    if !fs.is_empty() {
        println!("    files:");
        for e in &fs.added {
            println!("      + {}", mask_secrets(&fmt_entry(e), secrets));
        }
        for e in &fs.removed {
            println!("      - {}", mask_secrets(&fmt_entry(e), secrets));
        }
        // Modified lists can get long; show a bounded number.
        const MAX_MODIFIED: usize = 50;
        for p in fs.modified.iter().take(MAX_MODIFIED) {
            println!(
                "      ~ {}",
                mask_secrets(&p.display().to_string(), secrets)
            );
        }
        if fs.modified.len() > MAX_MODIFIED {
            println!(
                "      … and {} more modified",
                fs.modified.len() - MAX_MODIFIED
            );
        }
    }
    if fs.truncated {
        println!("    (filesystem diff skipped: workspace exceeds {MAX_DIFF_FILES} files)");
    }
}

fn fmt_entry(entry: &Entry) -> String {
    match entry {
        Entry::File(p) => p.display().to_string(),
        Entry::Dir(p, n) => format!("{}/ ({n} files)", p.display()),
    }
}

/// Truncate a long value for single-line display.
fn clip(s: &str) -> String {
    const MAX: usize = 80;
    let one_line = s.replace('\n', "⏎");
    if one_line.chars().count() > MAX {
        let kept: String = one_line.chars().take(MAX).collect();
        format!("{kept}…")
    } else {
        one_line
    }
}

/// Outputs harvested from a child's stdout `::workflow-command::` lines.
#[derive(Default)]
struct ScanResult {
    /// `::set-output name=NAME::VALUE` pairs, in emission order.
    outputs: Vec<(String, String)>,
}

/// Run `cmd`, streaming the child's stdout to ours line-by-line while parsing
/// GitHub `::workflow commands::`. `set-output` is harvested; `add-mask` masks
/// the rest of this run's stream; `error`/`warning`/`notice`/`group`/`debug` are
/// rendered; deprecated stdout `set-env`/`add-path` are ignored (their file
/// channels are honored instead); every other line prints through, with known
/// secrets and any `add-mask` values masked. stderr is left inherited (the
/// runner reads commands only from stdout). Piping stdout also matches GitHub,
/// where a step's stdout is not a TTY.
fn run_capturing(
    mut cmd: Command,
    secrets: &IndexMap<String, String>,
) -> Result<(ExitStatus, ScanResult)> {
    cmd.stdout(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to launch step process: {e}"))?;
    let stdout = child.stdout.take().expect("stdout was piped");

    let mut masks: Vec<String> = secrets.values().filter(|v| v.len() >= 4).cloned().collect();
    masks.sort_by_key(|v| std::cmp::Reverse(v.len()));

    let mut result = ScanResult::default();
    let handle = std::io::stdout();
    let mut reader = BufReader::new(stdout);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        // Read raw bytes so non-UTF-8 output degrades to lossy text rather than
        // aborting the step.
        let n = reader
            .read_until(b'\n', &mut buf)
            .context("reading step stdout")?;
        if n == 0 {
            break;
        }
        let cow = String::from_utf8_lossy(&buf);
        let mut line: &str = cow.as_ref();
        if let Some(s) = line.strip_suffix('\n') {
            line = s;
        }
        if let Some(s) = line.strip_suffix('\r') {
            line = s;
        }

        if let Some(command) = wfcmd::parse(line) {
            match command.name.as_str() {
                "set-output" => {
                    if let Some(name) = command.param("name") {
                        result
                            .outputs
                            .push((name.to_string(), command.message.clone()));
                    }
                    continue;
                }
                // Parsed but unused until pre/post hooks land.
                "save-state" => continue,
                "add-mask" => {
                    if command.message.len() >= 4 {
                        masks.push(command.message.clone());
                        masks.sort_by_key(|v| std::cmp::Reverse(v.len()));
                    }
                    continue;
                }
                // Disabled on GitHub as stdout commands; the file channels win.
                "set-env" | "add-path" | "echo" | "stop-commands" => continue,
                "error" | "warning" | "notice" => {
                    render_annotation(&handle, &command, &masks);
                    continue;
                }
                "group" => {
                    let mut o = handle.lock();
                    let _ = writeln!(o, "  ▸ {}", mask_line(&command.message, &masks));
                    continue;
                }
                "endgroup" => continue,
                "debug" => {
                    let mut o = handle.lock();
                    let _ = writeln!(o, "  ::debug:: {}", mask_line(&command.message, &masks));
                    continue;
                }
                // An unrecognized command name: fall through and print it raw.
                _ => {}
            }
        }
        let mut o = handle.lock();
        let _ = writeln!(o, "{}", mask_line(line, &masks));
    }

    let status = child
        .wait()
        .map_err(|e| anyhow::anyhow!("waiting for step process: {e}"))?;
    Ok((status, result))
}

/// Render an `error`/`warning`/`notice` annotation with its `file:line` if given.
fn render_annotation(handle: &std::io::Stdout, command: &wfcmd::Command, masks: &[String]) {
    let label = match command.name.as_str() {
        "error" => "✗ error",
        "warning" => "⚠ warning",
        _ => "notice",
    };
    let loc = match (command.param("file"), command.param("line")) {
        (Some(f), Some(l)) => format!(" ({f}:{l})"),
        (Some(f), None) => format!(" ({f})"),
        _ => String::new(),
    };
    let mut o = handle.lock();
    let _ = writeln!(o, "  {label}{loc}: {}", mask_line(&command.message, masks));
}

/// Replace each masked value in a passthrough line with `***` (longest first, so
/// a secret that is a substring of another can't leave the longer one exposed).
fn mask_line(s: &str, masks: &[String]) -> String {
    let mut out = s.to_string();
    for m in masks {
        out = out.replace(m.as_str(), "***");
    }
    out
}

/// Build the command and run it, capturing stdout for workflow commands.
#[allow(clippy::too_many_arguments)]
fn spawn_step(
    shell: &str,
    script: &str,
    cwd: &Path,
    step_env: &IndexMap<String, String>,
    path_additions: &[String],
    github: &Value,
    runner: &Value,
    env_file: &NamedTempFile,
    out_file: &NamedTempFile,
    path_file: &NamedTempFile,
    summary_file: &NamedTempFile,
    secrets: &IndexMap<String, String>,
) -> Result<(ExitStatus, ScanResult)> {
    let mut script_file = NamedTempFile::new().context("creating step script file")?;
    script_file
        .write_all(script.as_bytes())
        .context("writing step script")?;
    script_file.flush().ok();

    let (program, args) = shell_command(shell, script_file.path());
    let mut cmd = Command::new(&program);
    cmd.args(&args).current_dir(cwd);

    apply_common_env(&mut cmd, step_env, path_additions, github, runner, cwd);

    // Reserved channel files (set after common env so a workflow can't clobber them).
    cmd.env("GITHUB_ENV", env_file.path());
    cmd.env("GITHUB_OUTPUT", out_file.path());
    cmd.env("GITHUB_PATH", path_file.path());
    cmd.env("GITHUB_STEP_SUMMARY", summary_file.path());

    // `script_file` stays alive until `run_capturing` returns (the child has
    // exited by then), so the shell can still read it.
    run_capturing(cmd, secrets).with_context(|| format!("launching shell `{program}` for the step"))
}

/// The results of running one shell script: its exit status plus whatever it
/// wrote back through the `$GITHUB_ENV`/`$GITHUB_PATH`/`$GITHUB_OUTPUT` channels.
struct StepIo {
    status: std::process::ExitStatus,
    env_additions: Vec<(String, String)>,
    path_additions: Vec<String>,
    outputs: Vec<(String, String)>,
}

/// Run a script through the shell with fresh channel files, and read the channels
/// back. Shared by ordinary `run:` steps and composite-action steps. Outputs
/// combine the `$GITHUB_OUTPUT` file with any stdout `::set-output::` commands.
#[allow(clippy::too_many_arguments)]
fn execute_script(
    shell: &str,
    script: &str,
    cwd: &Path,
    step_env: &IndexMap<String, String>,
    path: &[String],
    github: &Value,
    runner: &Value,
    secrets: &IndexMap<String, String>,
) -> Result<StepIo> {
    let env_file = NamedTempFile::new().context("creating GITHUB_ENV file")?;
    let out_file = NamedTempFile::new().context("creating GITHUB_OUTPUT file")?;
    let path_file = NamedTempFile::new().context("creating GITHUB_PATH file")?;
    let summary_file = NamedTempFile::new().context("creating GITHUB_STEP_SUMMARY file")?;

    let (status, scan) = spawn_step(
        shell,
        script,
        cwd,
        step_env,
        path,
        github,
        runner,
        &env_file,
        &out_file,
        &path_file,
        &summary_file,
        secrets,
    )?;

    let mut outputs = read_key_values(out_file.path())?;
    outputs.extend(scan.outputs);

    Ok(StepIo {
        status,
        env_additions: read_key_values(env_file.path())?,
        path_additions: read_path_additions(path_file.path())?,
        outputs,
    })
}

/// Apply the step's user env plus the standard runner variables — workspace, CI
/// markers, `RUNNER_*`, the `GITHUB_*` mirror of the github context, and `PATH`
/// additions. Shared by the step spawn and the interactive debug shell.
fn apply_common_env(
    cmd: &mut Command,
    step_env: &IndexMap<String, String>,
    path_additions: &[String],
    github: &Value,
    runner: &Value,
    cwd: &Path,
) {
    cmd.envs(step_env);
    cmd.env("GITHUB_WORKSPACE", cwd);
    cmd.env("CI", "true");
    cmd.env("GITHUB_ACTIONS", "true");
    if let Some(os) = object_get(runner, "os") {
        cmd.env("RUNNER_OS", os);
    }
    if let Some(arch) = object_get(runner, "arch") {
        cmd.env("RUNNER_ARCH", arch);
    }
    if let Some(temp) = object_get(runner, "temp") {
        cmd.env("RUNNER_TEMP", temp);
    }
    for (var, key) in [
        ("GITHUB_SHA", "sha"),
        ("GITHUB_REF", "ref"),
        ("GITHUB_REF_NAME", "ref_name"),
        ("GITHUB_EVENT_NAME", "event_name"),
    ] {
        if let Some(val) = object_get(github, key) {
            cmd.env(var, val);
        }
    }
    if !path_additions.is_empty() {
        let existing = std::env::var("PATH").unwrap_or_default();
        let joined = path_additions.join(":");
        let new_path = if existing.is_empty() {
            joined
        } else {
            format!("{joined}:{existing}")
        };
        cmd.env("PATH", new_path);
    }
}

/// Map a shell name to a program + argument list, matching the runner's defaults.
fn shell_command(shell: &str, script: &Path) -> (String, Vec<String>) {
    let script = script.to_string_lossy().into_owned();
    match shell {
        "bash" => (
            "bash".into(),
            vec![
                "--noprofile".into(),
                "--norc".into(),
                "-e".into(),
                "-o".into(),
                "pipefail".into(),
                script,
            ],
        ),
        "sh" => ("sh".into(), vec!["-e".into(), script]),
        "python" => ("python3".into(), vec![script]),
        // A custom shell template: `{0}` is the script path, else it's appended.
        other => {
            let mut parts = other.split_whitespace().map(String::from);
            let program = parts.next().unwrap_or_else(|| "bash".into());
            let mut args: Vec<String> = parts.collect();
            if let Some(slot) = args.iter_mut().find(|a| *a == "{0}") {
                *slot = script;
            } else {
                args.push(script);
            }
            (program, args)
        }
    }
}

fn resolve_cwd(step: &Step, job: &Job, wf: &Workflow, base: &Path) -> PathBuf {
    let wd = step
        .working_directory
        .as_deref()
        .or(job.defaults.working_directory.as_deref())
        .or(wf.defaults.working_directory.as_deref());
    match wd {
        Some(dir) => {
            let p = Path::new(dir);
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                base.join(p)
            }
        }
        None => base.to_path_buf(),
    }
}

/// Interpolate and add a set of env entries to the job state, in order.
fn layer_env(
    state: &mut JobState,
    entries: &IndexMap<String, String>,
    github: &Value,
    runner: &Value,
    secrets: &IndexMap<String, String>,
) -> Result<()> {
    for (k, v) in entries {
        let mut ctx = make_context(
            &state.env,
            github,
            runner,
            &state.steps,
            secrets,
            state.status,
        );
        ctx.vars
            .insert("matrix".to_string(), Value::Object(state.matrix.clone()));
        let value = expr::interpolate(v, &ctx)?;
        state.env.insert(k.clone(), value);
    }
    Ok(())
}

/// Evaluate an `if:` condition (step- or job-level). GitHub implicitly wraps a
/// condition with `success()` unless it already references a status function —
/// so a condition that isn't a status check only runs when nothing has failed.
fn eval_condition(raw: &str, ctx: &Context) -> Result<bool> {
    let inner = strip_expr_wrapper(raw.trim());
    let value = expr::evaluate(inner, ctx)?;
    if expr::references_status_function(inner)? {
        Ok(value.is_truthy())
    } else {
        Ok(ctx.status == JobStatus::Success && value.is_truthy())
    }
}

/// Strip a single surrounding `${{ … }}` wrapper from an `if:` string, if present.
fn strip_expr_wrapper(s: &str) -> &str {
    if let Some(rest) = s.strip_prefix("${{")
        && let Some(inner) = rest.strip_suffix("}}")
    {
        return inner.trim();
    }
    s
}

fn step_continues_on_error(step: &Step, ctx: &Context) -> Result<bool> {
    match &step.continue_on_error {
        Conditional::Bool(b) => Ok(*b),
        Conditional::Expr(e) => Ok(expr::evaluate(e, ctx)?.is_truthy()),
    }
}

fn record_step(
    state: &mut JobState,
    step: &Step,
    outcome: &str,
    conclusion: &str,
    outputs: IndexMap<String, String>,
) {
    let Some(id) = &step.id else { return };
    let outputs_obj: IndexMap<String, Value> = outputs
        .into_iter()
        .map(|(k, v)| (k, Value::Str(v)))
        .collect();
    let mut obj = IndexMap::new();
    obj.insert("outputs".to_string(), Value::Object(outputs_obj));
    obj.insert("outcome".to_string(), Value::Str(outcome.to_string()));
    obj.insert("conclusion".to_string(), Value::Str(conclusion.to_string()));
    state.steps.insert(id.clone(), Value::Object(obj));
}

fn make_context(
    env: &IndexMap<String, String>,
    github: &Value,
    runner: &Value,
    steps: &IndexMap<String, Value>,
    secrets: &IndexMap<String, String>,
    status: JobStatus,
) -> Context {
    make_context_with_inputs(
        env,
        github,
        runner,
        steps,
        secrets,
        &IndexMap::new(),
        status,
    )
}

/// Like [`make_context`] but also exposes an `inputs` context (for composite
/// action steps).
#[allow(clippy::too_many_arguments)]
fn make_context_with_inputs(
    env: &IndexMap<String, String>,
    github: &Value,
    runner: &Value,
    steps: &IndexMap<String, Value>,
    secrets: &IndexMap<String, String>,
    inputs: &IndexMap<String, String>,
    status: JobStatus,
) -> Context {
    let mut job = IndexMap::new();
    job.insert(
        "status".to_string(),
        Value::Str(status_str(status).to_string()),
    );

    let mut vars = IndexMap::new();
    vars.insert("env".to_string(), str_object(env));
    vars.insert("github".to_string(), github.clone());
    vars.insert("runner".to_string(), runner.clone());
    vars.insert("job".to_string(), Value::Object(job));
    vars.insert("steps".to_string(), Value::Object(steps.clone()));
    vars.insert("secrets".to_string(), str_object(secrets));
    vars.insert("inputs".to_string(), str_object(inputs));
    Context { vars, status }
}

/// A `String -> String` map as a `Value::Object` of string values.
fn str_object(map: &IndexMap<String, String>) -> Value {
    Value::Object(
        map.iter()
            .map(|(k, v)| (k.clone(), Value::Str(v.clone())))
            .collect(),
    )
}

fn status_str(status: JobStatus) -> &'static str {
    match status {
        JobStatus::Success => "success",
        JobStatus::Failure => "failure",
        JobStatus::Cancelled => "cancelled",
    }
}

fn read_key_values(path: &Path) -> Result<Vec<(String, String)>> {
    let content = std::fs::read_to_string(path).unwrap_or_default();
    envfile::parse_key_values(&content)
}

fn read_path_additions(path: &Path) -> Result<Vec<String>> {
    let content = std::fs::read_to_string(path).unwrap_or_default();
    Ok(envfile::parse_path_additions(&content))
}

/// Determine the job execution order: a single job when filtered, otherwise a
/// topological order over `needs` (preserving file order among ready jobs).
fn job_order(wf: &Workflow, only: Option<&str>) -> Result<Vec<String>> {
    if let Some(j) = only {
        if !wf.jobs.contains_key(j) {
            let available = wf.jobs.keys().cloned().collect::<Vec<_>>().join(", ");
            bail!("job `{j}` not found; available jobs: {available}");
        }
        return Ok(vec![j.to_string()]);
    }

    let ids: Vec<String> = wf.jobs.keys().cloned().collect();
    let mut done: HashSet<String> = HashSet::new();
    let mut order = Vec::new();
    while order.len() < ids.len() {
        let mut progressed = false;
        for id in &ids {
            if done.contains(id) {
                continue;
            }
            let ready = wf.jobs[id]
                .needs
                .iter()
                .all(|n| !wf.jobs.contains_key(n) || done.contains(n));
            if ready {
                order.push(id.clone());
                done.insert(id.clone());
                progressed = true;
            }
        }
        if !progressed {
            bail!("job dependency cycle detected among `needs`");
        }
    }
    Ok(order)
}

fn build_github(workspace: &Path) -> Value {
    let mut g = IndexMap::new();
    g.insert(
        "workspace".to_string(),
        Value::Str(workspace.to_string_lossy().into_owned()),
    );
    // A local run simulates a push by default.
    g.insert("event_name".to_string(), Value::Str("push".to_string()));
    if let Some(sha) = git(workspace, &["rev-parse", "HEAD"]) {
        g.insert("sha".to_string(), Value::Str(sha));
    }
    if let Some(branch) = git(workspace, &["rev-parse", "--abbrev-ref", "HEAD"]) {
        g.insert("ref_name".to_string(), Value::Str(branch.clone()));
        g.insert(
            "ref".to_string(),
            Value::Str(format!("refs/heads/{branch}")),
        );
        g.insert("ref_type".to_string(), Value::Str("branch".to_string()));
    }
    Value::Object(g)
}

fn build_runner() -> Value {
    let os = if cfg!(target_os = "linux") {
        "Linux"
    } else if cfg!(target_os = "macos") {
        "macOS"
    } else if cfg!(target_os = "windows") {
        "Windows"
    } else {
        "Unknown"
    };
    let arch = if cfg!(target_arch = "x86_64") {
        "X64"
    } else if cfg!(target_arch = "aarch64") {
        "ARM64"
    } else {
        std::env::consts::ARCH
    };
    let mut r = IndexMap::new();
    r.insert("os".to_string(), Value::Str(os.to_string()));
    r.insert("arch".to_string(), Value::Str(arch.to_string()));
    r.insert(
        "temp".to_string(),
        Value::Str(std::env::temp_dir().to_string_lossy().into_owned()),
    );
    Value::Object(r)
}

/// Run a git command in `dir`, returning trimmed stdout on success.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

fn object_get(v: &Value, key: &str) -> Option<String> {
    match v {
        Value::Object(m) => match m.get(key) {
            Some(Value::Str(s)) => Some(s.clone()),
            _ => None,
        },
        _ => None,
    }
}

fn step_label(step: &Step) -> String {
    if let Some(name) = &step.name {
        return name.clone();
    }
    match &step.action {
        StepAction::Run { script } => script
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("")
            .to_string(),
        StepAction::Uses { action, .. } => action.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(step_all: bool, breakpoints: &[&str]) -> RunOptions {
        RunOptions {
            job: None,
            workspace: ".".into(),
            step_all,
            breakpoints: breakpoints.iter().map(|s| s.to_string()).collect(),
            secrets: IndexMap::new(),
        }
    }

    #[test]
    fn secrets_are_masked_in_output() {
        let mut secrets = IndexMap::new();
        secrets.insert("TOKEN".to_string(), "s3cr3t-value".to_string());
        assert_eq!(
            mask_secrets("using s3cr3t-value here", &secrets),
            "using *** here"
        );
        // Very short values aren't masked (avoids garbling common text).
        let mut short = IndexMap::new();
        short.insert("X".to_string(), "ab".to_string());
        assert_eq!(mask_secrets("ab cd", &short), "ab cd");
    }

    #[test]
    fn overlapping_secrets_are_fully_masked() {
        // A shorter secret that is a substring of a longer one (inserted first)
        // must NOT leave the longer one partially unmasked (`***XY99`).
        let mut secrets = IndexMap::new();
        secrets.insert("SHORT".to_string(), "secret".to_string());
        secrets.insert("LONG".to_string(), "secretXY99".to_string());
        assert_eq!(mask_secrets("token=secretXY99", &secrets), "token=***");
    }

    #[test]
    fn should_pause_respects_step_all_and_breakpoints() {
        let wf = crate::parse::parse_str(
            "jobs:\n  j:\n    runs-on: x\n    steps:\n      - id: build\n        run: 'true'\n      - run: 'true'\n",
        )
        .unwrap();
        let steps = &wf.jobs["j"].steps;
        let with_id = &steps[0];
        let no_id = &steps[1];

        // --step pauses everywhere.
        assert!(should_pause_at(with_id, &opts(true, &[])));
        assert!(should_pause_at(no_id, &opts(true, &[])));
        // --break pauses only at the matching id.
        assert!(should_pause_at(with_id, &opts(false, &["build"])));
        assert!(!should_pause_at(no_id, &opts(false, &["build"])));
        // Neither flag → never pause.
        assert!(!should_pause_at(with_id, &opts(false, &[])));
    }

    fn ctx(status: JobStatus) -> Context {
        let mut vars = IndexMap::new();
        let mut github = IndexMap::new();
        github.insert("ref".to_string(), Value::Str("refs/heads/main".to_string()));
        vars.insert("github".to_string(), Value::Object(github));
        Context { vars, status }
    }

    fn action_input(default: Option<&str>, required: bool) -> crate::model::ActionInput {
        crate::model::ActionInput {
            default: default.map(str::to_string),
            required,
        }
    }

    fn matrix(text: &str) -> crate::model::Matrix {
        let wf = crate::parse::parse_str(&format!(
            "jobs:\n  j:\n    runs-on: x\n    strategy:\n{}\n    steps: [{{ run: 'true' }}]\n",
            text.lines()
                .map(|l| format!("      {l}"))
                .collect::<Vec<_>>()
                .join("\n")
        ))
        .unwrap();
        wf.jobs["j"]
            .strategy
            .clone()
            .unwrap()
            .matrix
            .clone()
            .unwrap()
    }

    fn combo_strings(combos: &[IndexMap<String, String>]) -> Vec<String> {
        combos
            .iter()
            .map(|c| {
                c.iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .collect()
    }

    #[test]
    fn matrix_cartesian_exclude_include() {
        let m = matrix("matrix:\n  os: [linux, mac]\n  v: [1, 2]");
        assert_eq!(
            combo_strings(&expand_matrix(&m)),
            vec!["os=linux,v=1", "os=linux,v=2", "os=mac,v=1", "os=mac,v=2"]
        );

        // exclude removes a combination.
        let m = matrix(
            "matrix:\n  os: [linux, mac]\n  v: [1, 2]\n  exclude:\n    - os: mac\n      v: 1",
        );
        assert_eq!(
            combo_strings(&expand_matrix(&m)),
            vec!["os=linux,v=1", "os=linux,v=2", "os=mac,v=2"]
        );

        // include with only new keys adds them to every combination.
        let m = matrix("matrix:\n  v: [1, 2]\n  include:\n    - extra: x");
        assert_eq!(
            combo_strings(&expand_matrix(&m)),
            vec!["v=1,extra=x", "v=2,extra=x"]
        );

        // include that matches no combination is appended as a new one.
        let m = matrix("matrix:\n  v: [1]\n  include:\n    - v: 9\n      note: added");
        assert_eq!(
            combo_strings(&expand_matrix(&m)),
            vec!["v=1", "v=9,note=added"]
        );
    }

    #[test]
    fn matrix_includes_only_match_base_combinations() {
        // Two includes that each fail to match the base product (os is never
        // `mac`) must stay DISTINCT — the second must not merge into the combo
        // the first appended. GitHub matches includes against the base product
        // only, so this yields 4 combinations, not 3.
        let m = matrix(
            "matrix:\n  os: [linux, win]\n  include:\n    - os: mac\n      gpu: nvidia\n    - os: mac\n      cuda: '12'",
        );
        assert_eq!(
            combo_strings(&expand_matrix(&m)),
            vec!["os=linux", "os=win", "os=mac,gpu=nvidia", "os=mac,cuda=12"]
        );
    }

    #[test]
    fn matrix_matches_github_include_reference_example() {
        // The canonical example from GitHub's matrix docs, which exercises every
        // include rule: add-to-all, match-subset, add-new-key, and non-matching
        // includes becoming their own combinations.
        let m = matrix(
            "matrix:\n  fruit: [apple, pear]\n  animal: [cat, dog]\n  include:\n    - color: green\n    - color: pink\n      animal: cat\n    - fruit: apple\n      shape: circle\n    - fruit: banana\n    - fruit: banana\n      animal: cat",
        );
        assert_eq!(
            combo_strings(&expand_matrix(&m)),
            vec![
                "fruit=apple,animal=cat,color=pink,shape=circle",
                "fruit=apple,animal=dog,color=green,shape=circle",
                "fruit=pear,animal=cat,color=pink",
                "fruit=pear,animal=dog,color=green",
                "fruit=banana",
                "fruit=banana,animal=cat",
            ]
        );
    }

    #[test]
    fn input_env_vars_uppercase_space_and_hyphen() {
        let mut inputs = IndexMap::new();
        inputs.insert("my-input".to_string(), "a".to_string());
        inputs.insert("two words".to_string(), "b".to_string());
        let env = input_env_vars(&inputs);
        // Hyphens are preserved (matches the runner); spaces become underscores.
        assert_eq!(env["INPUT_MY-INPUT"], "a");
        assert_eq!(env["INPUT_TWO_WORDS"], "b");
    }

    #[test]
    fn resolve_inputs_defaults_required_and_passthrough() {
        let mut inputs = IndexMap::new();
        inputs.insert("base".to_string(), action_input(Some("hi"), false));
        // A default that references an earlier input.
        inputs.insert(
            "derived".to_string(),
            action_input(Some("${{ inputs.base }}-x"), false),
        );
        let def = ActionDef {
            inputs,
            outputs: IndexMap::new(),
            runs: Runs::Composite { steps: Vec::new() },
        };

        // No `with`: defaults apply, and `derived` sees `base`.
        let empty = make_context(
            &IndexMap::new(),
            &Value::Null,
            &Value::Null,
            &IndexMap::new(),
            &IndexMap::new(),
            JobStatus::Success,
        );
        let mut with = IndexMap::new();
        with.insert("extra".to_string(), "passed".to_string()); // undeclared → passthrough
        let resolved = resolve_inputs(&def, &with, &empty).unwrap();
        assert_eq!(resolved["base"], "hi");
        assert_eq!(resolved["derived"], "hi-x");
        assert_eq!(resolved["extra"], "passed");
    }

    #[test]
    fn resolve_inputs_errors_on_missing_required() {
        let mut inputs = IndexMap::new();
        inputs.insert("token".to_string(), action_input(None, true));
        let def = ActionDef {
            inputs,
            outputs: IndexMap::new(),
            runs: Runs::Composite { steps: Vec::new() },
        };
        let empty = make_context(
            &IndexMap::new(),
            &Value::Null,
            &Value::Null,
            &IndexMap::new(),
            &IndexMap::new(),
            JobStatus::Success,
        );
        let err = resolve_inputs(&def, &IndexMap::new(), &empty).unwrap_err();
        assert!(format!("{err:#}").contains("required"));
    }

    #[test]
    fn resolve_action_classifies_references() {
        // These paths never touch the network (remote refs are covered by
        // fetch::parse_remote's unit tests).
        let ws = Path::new("/nonexistent-workspace-xyz");
        // A bare image is a Docker action.
        assert!(matches!(
            resolve_action("docker://alpine", ws).unwrap(),
            ResolvedAction::Docker(_, _)
        ));
        assert!(matches!(
            resolve_action("not-a-valid-ref", ws).unwrap(),
            ResolvedAction::Unsupported(_)
        ));
        // Local but missing action.yml → a clean error, not a panic.
        assert!(resolve_action("./missing-action", ws).is_err());
    }

    #[test]
    fn if_condition_implicitly_requires_success() {
        // No status function → wrapped with success(): true only when job is ok.
        assert!(
            eval_condition("github.ref == 'refs/heads/main'", &ctx(JobStatus::Success)).unwrap()
        );
        assert!(
            !eval_condition("github.ref == 'refs/heads/main'", &ctx(JobStatus::Failure)).unwrap()
        );
    }

    #[test]
    fn if_condition_with_status_function_is_not_wrapped() {
        // always() runs regardless of status; failure() only on failure.
        assert!(eval_condition("always()", &ctx(JobStatus::Failure)).unwrap());
        assert!(eval_condition("failure()", &ctx(JobStatus::Failure)).unwrap());
        assert!(!eval_condition("success()", &ctx(JobStatus::Failure)).unwrap());
    }

    #[test]
    fn if_wrapper_stripping() {
        assert_eq!(strip_expr_wrapper("${{ success() }}"), "success()");
        assert_eq!(strip_expr_wrapper("github.ref"), "github.ref");
    }

    #[test]
    fn status_detection_is_structural_not_textual() {
        use expr::references_status_function as refs;
        assert!(refs("always()").unwrap());
        assert!(refs("success() && github.ref == 'x'").unwrap());
        assert!(refs("contains(x, always())").unwrap()); // called as an argument
        // A property named "success" is NOT the function.
        assert!(!refs("steps.success.outputs.x == '1'").unwrap());
        // A string literal that merely spells a function name is NOT a call —
        // this is what the old substring scan got wrong.
        assert!(!refs("env.MODE == 'always()'").unwrap());
        assert!(!refs("contains(x, 'always(')").unwrap());
    }

    #[test]
    fn string_literal_condition_is_still_success_gated() {
        // `env.MODE == 'always()'` has no real status call, so after a failure it
        // must be skipped (implicit success() wrap) — not run.
        let mut c = ctx(JobStatus::Failure);
        if let Value::Object(m) = c.vars.get_mut("github").unwrap() {
            m.insert("mode".to_string(), Value::Str("always()".to_string()));
        }
        assert!(!eval_condition("github.mode == 'always()'", &c).unwrap());
    }

    #[test]
    fn shell_command_defaults() {
        let (p, a) = shell_command("bash", Path::new("/tmp/s.sh"));
        assert_eq!(p, "bash");
        assert_eq!(a.last().unwrap(), "/tmp/s.sh");
        assert!(a.contains(&"pipefail".to_string()));

        // Custom template substitutes {0}.
        let (p, a) = shell_command("pwsh -File {0}", Path::new("/tmp/s.ps1"));
        assert_eq!(p, "pwsh");
        assert_eq!(a, vec!["-File".to_string(), "/tmp/s.ps1".to_string()]);
    }

    #[test]
    fn job_order_is_topological() {
        // Build: c needs b, b needs a  →  a, b, c regardless of file order.
        let wf = crate::parse::parse_str(
            "jobs:\n  c:\n    needs: b\n    runs-on: x\n    steps: [{ run: 'true' }]\n  b:\n    needs: a\n    runs-on: x\n    steps: [{ run: 'true' }]\n  a:\n    runs-on: x\n    steps: [{ run: 'true' }]\n",
        )
        .unwrap();
        assert_eq!(job_order(&wf, None).unwrap(), vec!["a", "b", "c"]);
    }

    #[test]
    fn job_order_detects_cycles() {
        let wf = crate::parse::parse_str(
            "jobs:\n  a:\n    needs: b\n    runs-on: x\n    steps: [{ run: 'true' }]\n  b:\n    needs: a\n    runs-on: x\n    steps: [{ run: 'true' }]\n",
        )
        .unwrap();
        assert!(
            job_order(&wf, None)
                .unwrap_err()
                .to_string()
                .contains("cycle")
        );
    }
}
