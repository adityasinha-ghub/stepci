//! The native executor: run a workflow's jobs and `run:` steps directly on the
//! host — no Docker — evaluating `if:` conditions and interpolating `${{ }}` with
//! [`crate::expr`], and propagating `$GITHUB_ENV`/`$GITHUB_PATH`/`$GITHUB_OUTPUT`
//! between steps like the real runner.
//!
//! v0 scope: `run:` steps only. `uses:` steps are reported as skipped (native
//! action execution is deferred). Output streams straight through to the terminal.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result, bail};
use indexmap::IndexMap;
use tempfile::NamedTempFile;

use crate::envfile;
use crate::expr::{self, Context, JobStatus};
use crate::model::{Conditional, Job, Step, StepAction, Workflow};
use crate::value::Value;

/// Options for a run.
pub struct RunOptions {
    /// Run only this job (and don't auto-run its `needs`). `None` runs all jobs.
    pub job: Option<String>,
    /// The working directory used as `github.workspace` and the base for
    /// relative `working-directory`.
    pub workspace: PathBuf,
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

    let mut results: IndexMap<String, JobResult> = IndexMap::new();
    for id in &order {
        let job = &wf.jobs[id];
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

        let should_run = match &job.if_cond {
            Some(cond) => {
                let jctx = make_job_context(wf, &results, job, &github, &runner, ctx_status);
                eval_condition(cond, &jctx)?
            }
            None => needs_ok,
        };
        if !should_run {
            let why = if needs_ok {
                "if condition false"
            } else {
                "a needed job did not succeed"
            };
            println!("\n● job {id}: skipped ({why})");
            results.insert(id.clone(), JobResult::Skipped);
            continue;
        }

        let status = run_job(job, wf, &github, &runner, opts)?;
        let result = if status == JobStatus::Success {
            JobResult::Success
        } else {
            JobResult::Failed
        };
        results.insert(id.clone(), result);
    }

    let failed = results.values().any(|r| *r == JobResult::Failed);
    Ok(if failed { 1 } else { 0 })
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
fn make_job_context(
    wf: &Workflow,
    results: &IndexMap<String, JobResult>,
    job: &Job,
    github: &Value,
    runner: &Value,
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

    let env_obj: IndexMap<String, Value> = wf
        .env
        .iter()
        .map(|(k, v)| (k.clone(), Value::Str(v.clone())))
        .collect();
    let mut job_obj = IndexMap::new();
    job_obj.insert(
        "status".to_string(),
        Value::Str(status_str(status).to_string()),
    );

    let mut vars = IndexMap::new();
    vars.insert("github".to_string(), github.clone());
    vars.insert("runner".to_string(), runner.clone());
    vars.insert("env".to_string(), Value::Object(env_obj));
    vars.insert("needs".to_string(), Value::Object(needs));
    vars.insert("job".to_string(), Value::Object(job_obj));
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
    /// Running job status, which drives `if:` and the status functions.
    status: JobStatus,
}

fn run_job(
    job: &Job,
    wf: &Workflow,
    github: &Value,
    runner: &Value,
    opts: &RunOptions,
) -> Result<JobStatus> {
    let label = job.name.as_deref().unwrap_or(&job.id);
    println!("\n● job {} ({label})", job.id);

    let mut state = JobState {
        env: IndexMap::new(),
        path: Vec::new(),
        steps: IndexMap::new(),
        status: JobStatus::Success,
    };

    // Layer workflow then job env, interpolating each value as it's added so it
    // can reference earlier entries and the github/runner contexts.
    layer_env(&mut state, &wf.env, github, runner)?;
    layer_env(&mut state, &job.env, github, runner)?;

    for (i, step) in job.steps.iter().enumerate() {
        run_step(step, i + 1, &mut state, job, wf, github, runner, opts)
            .with_context(|| format!("in step {}", i + 1))?;
    }

    Ok(state.status)
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
) -> Result<()> {
    // Step env is layered over the job env for this step only.
    let mut step_env = state.env.clone();
    let ctx0 = make_context(&step_env, github, runner, &state.steps, state.status);
    for (k, v) in &step.env {
        step_env.insert(k.clone(), expr::interpolate(v, &ctx0)?);
    }
    let ctx = make_context(&step_env, github, runner, &state.steps, state.status);

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
        return Ok(());
    }

    let script = match &step.action {
        StepAction::Run { script } => expr::interpolate(script, &ctx)?,
        StepAction::Uses { action, .. } => {
            println!("skipped (uses: `{action}` — native actions not supported in v0)");
            record_step(state, step, "skipped", "skipped", IndexMap::new());
            return Ok(());
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

    // Channel files the step writes back through.
    let env_file = NamedTempFile::new().context("creating GITHUB_ENV file")?;
    let out_file = NamedTempFile::new().context("creating GITHUB_OUTPUT file")?;
    let path_file = NamedTempFile::new().context("creating GITHUB_PATH file")?;
    let summary_file = NamedTempFile::new().context("creating GITHUB_STEP_SUMMARY file")?;

    println!(); // end the "… " line before the step's own output streams
    let status = spawn_step(
        shell,
        &script,
        &cwd,
        &step_env,
        &state.path,
        github,
        runner,
        &env_file,
        &out_file,
        &path_file,
        &summary_file,
    )?;

    // Read back everything the step exported, regardless of exit status.
    let new_env = read_key_values(env_file.path())?;
    for (k, v) in &new_env {
        state.env.insert(k.clone(), v.clone());
    }
    for p in read_path_additions(path_file.path())? {
        state.path.insert(0, p); // most recently added wins
    }
    let outputs = read_key_values(out_file.path())?;

    let ok = status.success();
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

    let code = status.code().unwrap_or(-1);
    match (ok, conclusion) {
        (true, _) => println!("  ✓ step {number} ok"),
        (false, "success") => {
            println!("  ⚠ step {number} failed (exit {code}) — continue-on-error")
        }
        (false, _) => println!("  ✗ step {number} failed (exit {code})"),
    }
    Ok(())
}

/// Build the command and run it, streaming stdout/stderr to the terminal.
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
) -> Result<std::process::ExitStatus> {
    let mut script_file = NamedTempFile::new().context("creating step script file")?;
    script_file
        .write_all(script.as_bytes())
        .context("writing step script")?;
    script_file.flush().ok();

    let (program, args) = shell_command(shell, script_file.path());
    let mut cmd = Command::new(&program);
    cmd.args(&args).current_dir(cwd);

    // User-defined env for the step.
    cmd.envs(step_env);

    // Reserved runner variables (set last so a workflow can't clobber our channels).
    cmd.env("GITHUB_ENV", env_file.path());
    cmd.env("GITHUB_OUTPUT", out_file.path());
    cmd.env("GITHUB_PATH", path_file.path());
    cmd.env("GITHUB_STEP_SUMMARY", summary_file.path());
    cmd.env("GITHUB_WORKSPACE", cwd);
    cmd.env("CI", "true");
    cmd.env("GITHUB_ACTIONS", "true");
    if let Some(os) = object_get(runner, "os") {
        cmd.env("RUNNER_OS", os);
    }
    if let Some(arch) = object_get(runner, "arch") {
        cmd.env("RUNNER_ARCH", arch);
    }
    // Mirror the github context into the standard GITHUB_* variables steps read.
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

    // Prepend GITHUB_PATH additions to PATH.
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

    let status = cmd
        .status()
        .map_err(|e| anyhow::anyhow!("failed to launch shell `{program}` for the step: {e}"))?;
    Ok(status)
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
) -> Result<()> {
    for (k, v) in entries {
        let ctx = make_context(&state.env, github, runner, &state.steps, state.status);
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
    status: JobStatus,
) -> Context {
    let env_obj: IndexMap<String, Value> = env
        .iter()
        .map(|(k, v)| (k.clone(), Value::Str(v.clone())))
        .collect();
    let mut job = IndexMap::new();
    job.insert(
        "status".to_string(),
        Value::Str(status_str(status).to_string()),
    );

    let mut vars = IndexMap::new();
    vars.insert("env".to_string(), Value::Object(env_obj));
    vars.insert("github".to_string(), github.clone());
    vars.insert("runner".to_string(), runner.clone());
    vars.insert("job".to_string(), Value::Object(job));
    vars.insert("steps".to_string(), Value::Object(steps.clone()));
    vars.insert("secrets".to_string(), Value::Object(IndexMap::new()));
    Context { vars, status }
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

    fn ctx(status: JobStatus) -> Context {
        let mut vars = IndexMap::new();
        let mut github = IndexMap::new();
        github.insert("ref".to_string(), Value::Str("refs/heads/main".to_string()));
        vars.insert("github".to_string(), Value::Object(github));
        Context { vars, status }
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
