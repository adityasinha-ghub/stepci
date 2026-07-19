//! The parsed, validated workflow model that the executor and debugger consume.
//!
//! This is the *clean* representation: every raw-YAML quirk (scalar coercion,
//! string-or-list fields, the `run`/`uses` split) has already been resolved by
//! [`crate::parse`]. Maps preserve file order so output is deterministic.

use indexmap::IndexMap;

/// A whole workflow file, reduced to what the executor needs.
///
/// Note: workflow triggers (`on:`) are intentionally not modelled — a local
/// debugger runs jobs directly. (`on` is also the classic YAML 1.1 gotcha where
/// it resolves to boolean `true`; our parser reads YAML 1.2 so it stays a string
/// key, and either way it's simply ignored.) Unknown keys are ignored, not
/// rejected, so real-world workflows with fields we don't yet use still parse.
#[derive(Debug, Clone)]
pub struct Workflow {
    /// The workflow's `name:`, if given.
    pub name: Option<String>,
    /// Workflow-level environment, applied to every step.
    pub env: IndexMap<String, String>,
    /// Workflow-level `defaults.run` (shell / working-directory).
    pub defaults: Defaults,
    /// Jobs, keyed by job id, in file order.
    pub jobs: IndexMap<String, Job>,
}

/// `defaults.run` settings, which can appear at the workflow and job level.
#[derive(Debug, Clone, Default)]
pub struct Defaults {
    /// Overrides the shell used for `run:` steps.
    pub shell: Option<String>,
    /// Overrides the working directory for `run:` steps.
    pub working_directory: Option<String>,
}

/// A single job within a workflow.
#[derive(Debug, Clone)]
pub struct Job {
    /// The job id (its key under `jobs:`).
    pub id: String,
    /// The job's `name:`, if given.
    pub name: Option<String>,
    /// `runs-on`, normalized to a list of labels. Informational in v0 — steps
    /// run natively on the host regardless.
    pub runs_on: Vec<String>,
    /// Ids of jobs this one `needs`, normalized to a list.
    pub needs: Vec<String>,
    /// Job-level environment, layered over the workflow env.
    pub env: IndexMap<String, String>,
    /// Job-level `defaults.run`.
    pub defaults: Defaults,
    /// The raw `if:` condition expression, if any (evaluated in a later milestone).
    pub if_cond: Option<String>,
    /// The job's steps, in order.
    pub steps: Vec<Step>,
}

/// A single step: either a `run:` command or a `uses:` action.
#[derive(Debug, Clone)]
pub struct Step {
    /// The step's `id:`, used to reference its outputs.
    pub id: Option<String>,
    /// The step's `name:`, if given.
    pub name: Option<String>,
    /// The raw `if:` condition expression, if any.
    pub if_cond: Option<String>,
    /// Step-level environment, layered over job and workflow env.
    pub env: IndexMap<String, String>,
    /// Overrides the working directory for this step.
    pub working_directory: Option<String>,
    /// Overrides the shell for this step.
    pub shell: Option<String>,
    /// Whether a failure of this step should abort the job.
    pub continue_on_error: Conditional,
    /// What the step actually does.
    pub action: StepAction,
}

/// The two kinds of step body. GitHub requires exactly one per step.
#[derive(Debug, Clone)]
pub enum StepAction {
    /// A shell command (`run:`), executed natively.
    Run {
        /// The script body (may be multi-line).
        script: String,
    },
    /// A referenced action (`uses:`) plus its `with:` inputs.
    Uses {
        /// The action reference, e.g. `actions/checkout@v4`.
        action: String,
        /// Inputs passed via `with:`.
        with: IndexMap<String, String>,
    },
}

/// A value that may be a literal boolean or an unevaluated `${{ }}` expression.
///
/// Used for `continue-on-error`, which GitHub allows to be either.
#[derive(Debug, Clone)]
pub enum Conditional {
    /// A literal `true`/`false`.
    Bool(bool),
    /// A raw expression string, evaluated in a later milestone.
    Expr(String),
}

/// A parsed `action.yml` / `action.yaml` — the metadata for a `uses:` action.
#[derive(Debug, Clone)]
pub struct ActionDef {
    /// Declared inputs, keyed by name (with defaults / required flags).
    pub inputs: IndexMap<String, ActionInput>,
    /// Declared outputs, keyed by name.
    pub outputs: IndexMap<String, ActionOutput>,
    /// How the action runs.
    pub runs: Runs,
}

/// A declared action input.
#[derive(Debug, Clone)]
pub struct ActionInput {
    /// Default value (a `${{ }}` expression is allowed), if any.
    pub default: Option<String>,
    /// Whether the input is required.
    pub required: bool,
}

/// A declared action output.
#[derive(Debug, Clone)]
pub struct ActionOutput {
    /// For a composite action, the `${{ }}` expression producing the value.
    /// (`node`/`docker` actions set outputs at runtime, so this is `None`.)
    pub value: Option<String>,
}

/// How an action runs, from `runs.using`.
#[derive(Debug, Clone)]
pub enum Runs {
    /// A composite action: a list of steps run in-process.
    Composite {
        /// The nested steps.
        steps: Vec<Step>,
    },
    /// A JavaScript action (`node16`/`node20`/…). Executed in a later milestone.
    Node {
        /// The entry script (`runs.main`).
        main: String,
        /// Optional `pre`/`post` scripts.
        pre: Option<String>,
        post: Option<String>,
    },
    /// A Docker action.
    Docker {
        /// The image reference: `docker://…` (prebuilt) or `Dockerfile` (built).
        image: String,
        /// An entrypoint override (`runs.entrypoint`).
        entrypoint: Option<String>,
        /// Command args (`runs.args`), each a `${{ }}`-interpolatable string.
        args: Vec<String>,
        /// Extra container env (`runs.env`).
        env: IndexMap<String, String>,
    },
}
