//! Parse a GitHub Actions workflow file into the validated [`crate::model`].
//!
//! We deserialize into permissive *raw* structs (unknown keys ignored, scalars
//! left as [`serde_yaml::Value`]) and then convert into the clean model, doing
//! the fiddly normalization — scalar-to-string coercion, string-or-list fields,
//! the `run`/`uses` split — in one place with contextual error messages.

use anyhow::{Context, Result, bail};
use indexmap::IndexMap;
use serde::Deserialize;
use serde_yaml::Value as Yaml;
use std::path::Path;

use crate::model::{Conditional, Defaults, Job, Step, StepAction, Workflow};

/// Read and parse a workflow file from disk.
pub fn parse_file(path: &Path) -> Result<Workflow> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading workflow `{}`", path.display()))?;
    parse_str(&text).with_context(|| format!("in workflow `{}`", path.display()))
}

/// Parse a workflow from an in-memory string.
pub fn parse_str(text: &str) -> Result<Workflow> {
    if text.trim().is_empty() {
        bail!("workflow file is empty");
    }
    let raw: RawWorkflow = serde_yaml::from_str(text).context("invalid workflow YAML")?;
    raw.into_workflow()
}

// ---------------------------------------------------------------------------
// Raw (permissive) deserialization targets
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct RawWorkflow {
    name: Option<String>,
    #[serde(default)]
    env: IndexMap<String, Yaml>,
    #[serde(default)]
    defaults: RawDefaults,
    #[serde(default)]
    jobs: IndexMap<String, RawJob>,
}

#[derive(Deserialize, Default)]
struct RawDefaults {
    #[serde(default)]
    run: RawRunDefaults,
}

#[derive(Deserialize, Default)]
struct RawRunDefaults {
    shell: Option<String>,
    #[serde(rename = "working-directory")]
    working_directory: Option<String>,
}

#[derive(Deserialize)]
struct RawJob {
    name: Option<String>,
    #[serde(rename = "runs-on", default)]
    runs_on: Option<Yaml>,
    #[serde(default)]
    needs: Option<Yaml>,
    #[serde(default)]
    env: IndexMap<String, Yaml>,
    #[serde(default)]
    defaults: RawDefaults,
    #[serde(rename = "if")]
    if_cond: Option<String>,
    #[serde(default)]
    steps: Vec<RawStep>,
}

#[derive(Deserialize)]
struct RawStep {
    id: Option<String>,
    name: Option<String>,
    #[serde(rename = "if")]
    if_cond: Option<String>,
    #[serde(default)]
    env: IndexMap<String, Yaml>,
    #[serde(rename = "working-directory")]
    working_directory: Option<String>,
    shell: Option<String>,
    #[serde(rename = "continue-on-error", default)]
    continue_on_error: Option<Yaml>,
    run: Option<String>,
    uses: Option<String>,
    #[serde(default)]
    with: IndexMap<String, Yaml>,
}

// ---------------------------------------------------------------------------
// Raw -> clean conversion (with validation)
// ---------------------------------------------------------------------------

impl RawWorkflow {
    fn into_workflow(self) -> Result<Workflow> {
        if self.jobs.is_empty() {
            bail!("workflow defines no `jobs`");
        }
        let mut jobs = IndexMap::with_capacity(self.jobs.len());
        for (id, raw) in self.jobs {
            let job = raw
                .into_job(id.clone())
                .with_context(|| format!("in job `{id}`"))?;
            jobs.insert(id, job);
        }
        Ok(Workflow {
            name: self.name,
            env: scalar_map(self.env, "workflow `env`")?,
            defaults: self.defaults.into(),
            jobs,
        })
    }
}

impl RawJob {
    fn into_job(self, id: String) -> Result<Job> {
        let mut steps = Vec::with_capacity(self.steps.len());
        for (i, raw) in self.steps.into_iter().enumerate() {
            let step = raw
                .into_step()
                .with_context(|| format!("in step {}", i + 1))?;
            steps.push(step);
        }
        Ok(Job {
            id,
            name: self.name,
            runs_on: normalize_runs_on(self.runs_on),
            needs: string_or_list(self.needs).context("invalid `needs`")?,
            env: scalar_map(self.env, "job `env`")?,
            defaults: self.defaults.into(),
            if_cond: self.if_cond,
            steps,
        })
    }
}

impl RawStep {
    fn into_step(self) -> Result<Step> {
        let action = match (self.run, self.uses) {
            (Some(script), None) => StepAction::Run { script },
            (None, Some(action)) => StepAction::Uses {
                action,
                with: scalar_map(self.with, "`with`")?,
            },
            (Some(_), Some(_)) => {
                bail!("has both `run` and `uses`; a step must have exactly one")
            }
            (None, None) => bail!("has neither `run` nor `uses`; a step must have exactly one"),
        };
        Ok(Step {
            id: self.id,
            name: self.name,
            if_cond: self.if_cond,
            env: scalar_map(self.env, "step `env`")?,
            working_directory: self.working_directory,
            shell: self.shell,
            continue_on_error: normalize_conditional(self.continue_on_error)
                .context("invalid `continue-on-error`")?,
            action,
        })
    }
}

impl From<RawDefaults> for Defaults {
    fn from(d: RawDefaults) -> Self {
        Defaults {
            shell: d.run.shell,
            working_directory: d.run.working_directory,
        }
    }
}

// ---------------------------------------------------------------------------
// Normalization helpers
// ---------------------------------------------------------------------------

/// Coerce a YAML scalar (string/number/bool/null) into a string, matching how
/// GitHub stringifies `env`/`with` values. Non-scalars are an error.
fn scalar_to_string(v: &Yaml) -> Result<String> {
    match v {
        Yaml::Null => Ok(String::new()),
        Yaml::Bool(b) => Ok(b.to_string()),
        Yaml::Number(n) => Ok(n.to_string()),
        Yaml::String(s) => Ok(s.clone()),
        other => bail!("expected a scalar value, found a {}", kind_of(other)),
    }
}

/// Convert a map of YAML scalars into a `String -> String` map, preserving order.
fn scalar_map(map: IndexMap<String, Yaml>, ctx: &str) -> Result<IndexMap<String, String>> {
    let mut out = IndexMap::with_capacity(map.len());
    for (k, v) in map {
        let s = scalar_to_string(&v).with_context(|| format!("{ctx} value for `{k}`"))?;
        out.insert(k, s);
    }
    Ok(out)
}

/// Normalize a `string | [string]` field (e.g. `needs`) into a list.
fn string_or_list(v: Option<Yaml>) -> Result<Vec<String>> {
    match v {
        None | Some(Yaml::Null) => Ok(Vec::new()),
        Some(Yaml::String(s)) => Ok(vec![s]),
        Some(Yaml::Sequence(seq)) => seq.iter().map(scalar_to_string).collect(),
        Some(other) => bail!("expected a string or list, found a {}", kind_of(&other)),
    }
}

/// Best-effort normalization of `runs-on` (string | list | `{group, labels}`).
/// Informational in v0, so this never fails — an unrecognized shape yields `[]`.
fn normalize_runs_on(v: Option<Yaml>) -> Vec<String> {
    match v {
        Some(Yaml::String(s)) => vec![s],
        Some(Yaml::Sequence(seq)) => seq
            .iter()
            .filter_map(|e| scalar_to_string(e).ok())
            .collect(),
        // `{ group, labels }`: prefer labels, else fall back to the group so a
        // group-only runner isn't reported as "unspecified".
        Some(Yaml::Mapping(m)) => match m.get("labels") {
            Some(labels) => normalize_runs_on(Some(labels.clone())),
            None => m
                .get("group")
                .and_then(|g| scalar_to_string(g).ok())
                .map(|g| vec![g])
                .unwrap_or_default(),
        },
        _ => Vec::new(),
    }
}

/// Normalize `continue-on-error`, which may be a bool or a `${{ }}` expression.
///
/// serde_yaml follows YAML 1.2 (only `true`/`false` are booleans), but GitHub
/// reads workflows as YAML 1.1, where `yes`/`no`/`on`/`off`/`y`/`n` are also
/// booleans (the "Norway problem"). A boolean-typed field must match GitHub, so
/// we re-interpret those spellings here rather than mis-classifying them as an
/// expression string.
fn normalize_conditional(v: Option<Yaml>) -> Result<Conditional> {
    match v {
        None | Some(Yaml::Null) => Ok(Conditional::Bool(false)),
        Some(Yaml::Bool(b)) => Ok(Conditional::Bool(b)),
        Some(Yaml::String(s)) => Ok(bool_or_expr(s)),
        Some(other) => bail!(
            "must be a boolean or an expression, found a {}",
            kind_of(&other)
        ),
    }
}

/// Classify a string in a boolean-or-expression slot: a real `${{ }}` expression
/// stays an expression; a YAML 1.1 boolean literal becomes a bool; anything else
/// is left as an expression for the evaluator to resolve (or reject) later.
fn bool_or_expr(s: String) -> Conditional {
    if s.contains("${{") {
        return Conditional::Expr(s);
    }
    match s.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "y" => Conditional::Bool(true),
        "false" | "no" | "off" | "n" => Conditional::Bool(false),
        _ => Conditional::Expr(s),
    }
}

/// A human-readable name for a YAML value's shape, for error messages.
fn kind_of(v: &Yaml) -> &'static str {
    match v {
        Yaml::Null => "null",
        Yaml::Bool(_) => "boolean",
        Yaml::Number(_) => "number",
        Yaml::String(_) => "string",
        Yaml::Sequence(_) => "sequence",
        Yaml::Mapping(_) => "mapping",
        Yaml::Tagged(_) => "tagged value",
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Workflow {
        parse_str(text).expect("should parse")
    }

    #[test]
    fn parses_a_basic_run_workflow() {
        let wf = parse(
            r#"
name: Basic
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - name: Say hi
        run: echo hi
"#,
        );
        assert_eq!(wf.name.as_deref(), Some("Basic"));
        let job = wf.jobs.get("build").expect("build job");
        assert_eq!(job.runs_on, vec!["ubuntu-latest"]);
        assert_eq!(job.steps.len(), 1);
        match &job.steps[0].action {
            StepAction::Run { script } => assert_eq!(script.trim(), "echo hi"),
            other => panic!("expected run, got {other:?}"),
        }
    }

    #[test]
    fn coerces_scalar_env_values_to_strings() {
        let wf = parse(
            r#"
env:
  A_STRING: hello
  A_NUMBER: 42
  A_BOOL: true
  EMPTY:
jobs:
  j:
    runs-on: x
    steps:
      - run: 'true'
"#,
        );
        assert_eq!(wf.env["A_STRING"], "hello");
        assert_eq!(wf.env["A_NUMBER"], "42");
        assert_eq!(wf.env["A_BOOL"], "true");
        assert_eq!(wf.env["EMPTY"], "");
    }

    #[test]
    fn normalizes_needs_string_and_list() {
        let wf = parse(
            r#"
jobs:
  a:
    runs-on: x
    steps: [{ run: 'true' }]
  b:
    runs-on: x
    needs: a
    steps: [{ run: 'true' }]
  c:
    runs-on: x
    needs: [a, b]
    steps: [{ run: 'true' }]
"#,
        );
        assert_eq!(wf.jobs["a"].needs, Vec::<String>::new());
        assert_eq!(wf.jobs["b"].needs, vec!["a"]);
        assert_eq!(wf.jobs["c"].needs, vec!["a", "b"]);
    }

    #[test]
    fn parses_uses_with_inputs() {
        let wf = parse(
            r#"
jobs:
  j:
    runs-on: x
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0
          token: abc
"#,
        );
        match &wf.jobs["j"].steps[0].action {
            StepAction::Uses { action, with } => {
                assert_eq!(action, "actions/checkout@v4");
                assert_eq!(with["fetch-depth"], "0"); // number coerced
                assert_eq!(with["token"], "abc");
            }
            other => panic!("expected uses, got {other:?}"),
        }
    }

    #[test]
    fn rejects_step_with_both_run_and_uses() {
        let err = parse_str(
            r#"
jobs:
  j:
    runs-on: x
    steps:
      - run: echo hi
        uses: actions/checkout@v4
"#,
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("both `run` and `uses`"), "got: {msg}");
        assert!(msg.contains("step 1"), "should locate the step: {msg}");
    }

    #[test]
    fn rejects_step_with_neither_run_nor_uses() {
        let err = parse_str(
            r#"
jobs:
  j:
    runs-on: x
    steps:
      - name: does nothing
"#,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("neither `run` nor `uses`"));
    }

    #[test]
    fn rejects_workflow_without_jobs() {
        let err = parse_str("name: no jobs here\n").unwrap_err();
        assert!(format!("{err:#}").contains("no `jobs`"));
    }

    #[test]
    fn rejects_empty_file() {
        let err = parse_str("   \n").unwrap_err();
        assert!(format!("{err:#}").contains("empty"));
    }

    #[test]
    fn handles_continue_on_error_bool_and_expr() {
        let wf = parse(
            r#"
jobs:
  j:
    runs-on: x
    steps:
      - run: 'true'
        continue-on-error: true
      - run: 'true'
        continue-on-error: ${{ github.event_name == 'push' }}
      - run: 'true'
      - run: 'true'
        continue-on-error: 'yes'
      - run: 'true'
        continue-on-error: 'off'
      - run: 'true'
        continue-on-error: 'no'
"#,
        );
        let steps = &wf.jobs["j"].steps;
        assert!(matches!(
            steps[0].continue_on_error,
            Conditional::Bool(true)
        ));
        assert!(matches!(steps[1].continue_on_error, Conditional::Expr(_)));
        assert!(matches!(
            steps[2].continue_on_error,
            Conditional::Bool(false)
        ));
        // serde_yaml uses YAML 1.2 (yes/on/off are strings) but GitHub reads
        // YAML 1.1, where these are booleans. We must match GitHub, not the lib.
        assert!(
            matches!(steps[3].continue_on_error, Conditional::Bool(true)),
            "`yes` should be Bool(true), got {:?}",
            steps[3].continue_on_error
        );
        assert!(
            matches!(steps[4].continue_on_error, Conditional::Bool(false)),
            "`off` should be Bool(false), got {:?}",
            steps[4].continue_on_error
        );
        assert!(
            matches!(steps[5].continue_on_error, Conditional::Bool(false)),
            "`no` should be Bool(false), got {:?}",
            steps[5].continue_on_error
        );
    }

    #[test]
    fn the_on_trigger_key_does_not_break_parsing() {
        // We don't model triggers (a local debugger runs jobs directly). Under
        // serde_yaml (YAML 1.2) `on` is a plain string key, so it's ignored like
        // any other unknown key; this test just guards that assumption for the
        // spellings real workflows use.
        for on in [
            "on: push",
            "on: [push, pull_request]",
            "\"on\": { push: {} }",
        ] {
            let wf = parse(&format!(
                "{on}\njobs:\n  j:\n    runs-on: x\n    steps: [{{ run: 'true' }}]\n"
            ));
            assert!(wf.jobs.contains_key("j"), "failed for `{on}`");
        }
    }

    #[test]
    fn runs_on_group_only_is_not_reported_as_unspecified() {
        let wf = parse(
            "jobs:\n  j:\n    runs-on:\n      group: my-runners\n    steps: [{ run: 'true' }]\n",
        );
        assert_eq!(wf.jobs["j"].runs_on, vec!["my-runners"]);
    }

    #[test]
    fn applies_defaults_and_step_overrides() {
        let wf = parse(
            r#"
defaults:
  run:
    shell: bash
    working-directory: ./root
jobs:
  j:
    runs-on: x
    defaults:
      run:
        shell: sh
    steps:
      - run: 'true'
        working-directory: ./sub
"#,
        );
        assert_eq!(wf.defaults.shell.as_deref(), Some("bash"));
        assert_eq!(wf.defaults.working_directory.as_deref(), Some("./root"));
        assert_eq!(wf.jobs["j"].defaults.shell.as_deref(), Some("sh"));
        assert_eq!(
            wf.jobs["j"].steps[0].working_directory.as_deref(),
            Some("./sub")
        );
    }

    #[test]
    fn reports_line_number_on_malformed_yaml() {
        let err = parse_str("jobs:\n  j:\n   bad: [unclosed\n").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("invalid workflow YAML"), "got: {msg}");
        // The whole point of this test: the underlying error must locate the
        // problem, not just say "something's wrong".
        assert!(
            msg.contains("line ") && msg.contains("column "),
            "error should report a line/column: {msg}"
        );
    }

    /// Verify on real data: parse this repository's own CI workflow.
    #[test]
    fn parses_this_repos_real_ci_workflow() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/.github/workflows/ci.yml");
        let wf = parse_file(Path::new(path)).expect("real ci.yml should parse");

        assert_eq!(wf.name.as_deref(), Some("CI"));
        assert_eq!(wf.env["CARGO_TERM_COLOR"], "always");

        let test = wf.jobs.get("test").expect("`test` job");
        assert_eq!(test.runs_on, vec!["ubuntu-latest"]);

        // First step checks out via a `uses` action...
        match &test.steps[0].action {
            StepAction::Uses { action, .. } => {
                assert!(action.starts_with("actions/checkout"), "got {action}")
            }
            other => panic!("expected uses, got {other:?}"),
        }
        // ...and a named `run` step ("Clippy") exists.
        let clippy = test
            .steps
            .iter()
            .find(|s| s.name.as_deref() == Some("Clippy"))
            .expect("a Clippy step");
        assert!(matches!(clippy.action, StepAction::Run { .. }));
    }
}
