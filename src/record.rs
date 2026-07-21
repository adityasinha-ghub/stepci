//! Persist each run as a re-openable **recording** — the per-step diffs and
//! metadata the executor already computes — so a finished run stops being a log
//! you scroll past once and becomes a durable object you can list and re-open
//! (`stepci runs` / `stepci show`).
//!
//! v0 stores the per-step *diffs and metadata* (what each step changed), not full
//! file contents — cheap, and enough to answer "what did step 7 do?" after the
//! fact. Full-content checkpoints (to materialize the exact world / re-run one
//! step) are a later milestone.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Bumped when the on-disk record shape changes, so stale records are ignored
/// rather than mis-read.
pub const FORMAT_VERSION: u32 = 1;

/// How many recent runs to keep on disk before pruning the oldest.
const MAX_RUNS: usize = 50;

/// A whole recorded run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    pub format_version: u32,
    pub stepci_version: String,
    /// The workflow file, as invoked.
    pub workflow: String,
    /// When the run started (Unix milliseconds) — also its id / sort key.
    pub started_unix_ms: u128,
    /// The process exit code the run produced.
    pub exit_code: i32,
    pub jobs: Vec<JobRecord>,
}

/// One job (one matrix combination) within a recorded run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRecord {
    pub id: String,
    pub name: Option<String>,
    /// The matrix suffix (e.g. `[linux, 18]`), or empty.
    pub matrix: String,
    /// `success` or `failure`.
    pub status: String,
    pub steps: Vec<StepRecord>,
}

/// One step's recorded outcome and diff.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StepRecord {
    pub number: usize,
    pub label: String,
    /// `run`, `uses:<ref>`, `artifact`, or `cache`.
    pub kind: String,
    /// `success`, `failure`, or `skipped`.
    pub outcome: String,
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub env_added: Vec<(String, String)>,
    #[serde(default)]
    pub env_changed: Vec<(String, String, String)>,
    #[serde(default)]
    pub env_removed: Vec<String>,
    #[serde(default)]
    pub path_added: Vec<String>,
    #[serde(default)]
    pub files_added: Vec<String>,
    #[serde(default)]
    pub files_removed: Vec<String>,
    #[serde(default)]
    pub files_modified: Vec<String>,
    #[serde(default)]
    pub files_truncated: bool,
    /// For a failing bash step, the "failed at line …" summary.
    #[serde(default)]
    pub failure: Option<String>,
    /// For a skipped step, the explanation lines.
    #[serde(default)]
    pub skip_reason: Vec<String>,
}

/// The directory holding all recordings (`~/.cache/stepci/runs`).
pub fn runs_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .ok_or_else(|| anyhow::anyhow!("no HOME directory for the run store"))?;
    Ok(PathBuf::from(home).join(".cache/stepci/runs"))
}

/// Write a recording to `~/.cache/stepci/runs/<started_unix_ms>/run.json`, then
/// prune the oldest so only the most recent [`MAX_RUNS`] remain.
pub fn save(record: &RunRecord) -> Result<()> {
    let dir = runs_dir()?.join(record.started_unix_ms.to_string());
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating run dir `{}`", dir.display()))?;
    let json = serde_json::to_string_pretty(record).context("serializing run record")?;
    std::fs::write(dir.join("run.json"), json)
        .with_context(|| format!("writing `{}`", dir.join("run.json").display()))?;
    prune()?;
    Ok(())
}

/// All recordings, newest first (records of a different format version, or
/// unreadable ones, are skipped rather than erroring the whole listing).
pub fn list() -> Result<Vec<RunRecord>> {
    let dir = runs_dir()?;
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut records = Vec::new();
    for entry in std::fs::read_dir(&dir).context("reading the run store")? {
        let path = entry?.path().join("run.json");
        if let Ok(text) = std::fs::read_to_string(&path)
            && let Ok(r) = serde_json::from_str::<RunRecord>(&text)
            && r.format_version == FORMAT_VERSION
        {
            records.push(r);
        }
    }
    records.sort_by_key(|r| std::cmp::Reverse(r.started_unix_ms));
    Ok(records)
}

/// The `n`-th most recent recording (1 = most recent).
pub fn nth_recent(n: usize) -> Result<Option<RunRecord>> {
    Ok(list()?.into_iter().nth(n.saturating_sub(1)))
}

/// Remove the oldest recordings beyond [`MAX_RUNS`].
fn prune() -> Result<()> {
    let dir = runs_dir()?;
    let mut dirs: Vec<(u128, PathBuf)> = std::fs::read_dir(&dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .filter_map(|p| {
            let ms = p.file_name()?.to_str()?.parse::<u128>().ok()?;
            Some((ms, p))
        })
        .collect();
    dirs.sort_by_key(|(ms, _)| *ms); // oldest first
    if dirs.len() > MAX_RUNS {
        for (_, old) in &dirs[..dirs.len() - MAX_RUNS] {
            let _ = std::fs::remove_dir_all(old);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_round_trips_through_json() {
        let rec = RunRecord {
            format_version: FORMAT_VERSION,
            stepci_version: "0.1.0".into(),
            workflow: "ci.yml".into(),
            started_unix_ms: 1_734_000_000_123,
            exit_code: 1,
            jobs: vec![JobRecord {
                id: "build".into(),
                name: Some("Build".into()),
                matrix: " [linux]".into(),
                status: "failure".into(),
                steps: vec![StepRecord {
                    number: 1,
                    label: "Compile".into(),
                    kind: "run".into(),
                    outcome: "failure".into(),
                    exit_code: Some(1),
                    env_added: vec![("V".into(), "1".into())],
                    failure: Some("failed at line 2: cp x y (exit 1)".into()),
                    ..Default::default()
                }],
            }],
        };
        let json = serde_json::to_string(&rec).unwrap();
        let back: RunRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.exit_code, 1);
        assert_eq!(back.format_version, FORMAT_VERSION);
        let step = &back.jobs[0].steps[0];
        assert_eq!(step.label, "Compile");
        assert_eq!(step.env_added, vec![("V".to_string(), "1".to_string())]);
        assert_eq!(
            step.failure.as_deref(),
            Some("failed at line 2: cp x y (exit 1)")
        );
        // Partial records (missing optional fields) still parse via serde default.
        let minimal = r#"{"format_version":1,"stepci_version":"x","workflow":"w",
            "started_unix_ms":1,"exit_code":0,
            "jobs":[{"id":"j","name":null,"matrix":"","status":"success",
            "steps":[{"number":1,"label":"s","kind":"run","outcome":"success","exit_code":null}]}]}"#;
        let parsed: RunRecord = serde_json::from_str(minimal).unwrap();
        assert!(parsed.jobs[0].steps[0].env_added.is_empty());
    }
}
