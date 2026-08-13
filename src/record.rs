//! Persist each run as a re-openable **recording** — so a finished run stops
//! being a log you scroll past once and becomes a durable object you can list,
//! re-open, compare, read, and trace (`stepci runs`/`show`/`diff`/`cat`/`why`).
//!
//! Each changed *individual* file is content-hashed (SHA-256, under a size cap)
//! and its bytes stored in a deduplicated content-addressed blob store — so a
//! diff is byte-accurate and can show the actual line-level change, `cat` can
//! print a file's recorded content, and `why` can trace a value/file to the
//! steps that produced it. Whole-workspace checkpoints (to materialize a step's
//! exact world or re-run one step) are still a later milestone.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Bumped when the on-disk record shape changes, so stale records are ignored
/// rather than mis-read.
pub const FORMAT_VERSION: u32 = 2;

/// Files larger than this aren't content-hashed (the diff falls back to
/// size+mtime for them) — bounds the per-step hashing cost.
const MAX_HASH_BYTES: u64 = 25 * 1024 * 1024;

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
    /// Files the step changed (added/modified/removed), with content hashes where
    /// available — so a diff can tell "changed to the same content" from a real
    /// content difference.
    #[serde(default)]
    pub files: Vec<FileChange>,
    #[serde(default)]
    pub files_truncated: bool,
    /// For a failing bash step, the "failed at line …" summary.
    #[serde(default)]
    pub failure: Option<String>,
    /// For a skipped step, the explanation lines.
    #[serde(default)]
    pub skip_reason: Vec<String>,
}

/// One file a step added, modified, or removed.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FileChange {
    /// Workspace-relative path (or the root of a collapsed new/removed directory).
    pub path: String,
    /// `added`, `modified`, or `removed`.
    pub status: String,
    /// For a collapsed directory, how many files it contains.
    #[serde(default)]
    pub dir_files: Option<usize>,
    /// The file's content hash (individual files under the size cap only).
    #[serde(default)]
    pub hash: Option<String>,
}

/// Content-hash a file **and** store its bytes in the content-addressed blob
/// store (deduplicated), returning the hash. `None` if it's missing, not a
/// regular file, or larger than [`MAX_HASH_BYTES`]. The blob lets `diff`/`cat`
/// show the file's actual content later.
pub fn hash_and_store(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_HASH_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let hash: String = Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if let Ok(dir) = blobs_dir() {
        let blob = dir.join(&hash);
        if !blob.exists() {
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::write(&blob, &bytes); // best-effort; hash is still useful
        }
    }
    Some(hash)
}

/// Load a stored blob's bytes by content hash.
pub fn load_blob(hash: &str) -> Option<Vec<u8>> {
    std::fs::read(blobs_dir().ok()?.join(hash)).ok()
}

/// A file's final recorded content within a run — the bytes as of the last step
/// that wrote it, if that content was stored as a blob.
pub fn file_content(run: &RunRecord, path: &str) -> Option<Vec<u8>> {
    let mut hash = None;
    for step in run.jobs.iter().flat_map(|j| &j.steps) {
        for f in &step.files {
            if f.path == path {
                hash = if f.status == "removed" {
                    None // last state: the file is gone
                } else {
                    f.hash.clone()
                };
            }
        }
    }
    load_blob(&hash?)
}

/// The root of the stepci cache (`~/.cache/stepci`).
fn cache_home() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .ok_or_else(|| anyhow::anyhow!("no HOME directory for the stepci cache"))?;
    Ok(PathBuf::from(home).join(".cache/stepci"))
}

/// The directory holding all recordings (`~/.cache/stepci/runs`).
pub fn runs_dir() -> Result<PathBuf> {
    Ok(cache_home()?.join("runs"))
}

/// The content-addressed blob store (`~/.cache/stepci/blobs`).
pub fn blobs_dir() -> Result<PathBuf> {
    Ok(cache_home()?.join("blobs"))
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
    let _ = gc_blobs(); // best-effort; a stale blob is harmless
    Ok(())
}

/// Delete blobs no longer referenced by any kept recording (mark-and-sweep over
/// the ≤ [`MAX_RUNS`] remaining runs).
fn gc_blobs() -> Result<()> {
    let dir = blobs_dir()?;
    if !dir.is_dir() {
        return Ok(());
    }
    let referenced: std::collections::HashSet<String> = list()?
        .iter()
        .flat_map(|r| &r.jobs)
        .flat_map(|j| &j.steps)
        .flat_map(|s| &s.files)
        .filter_map(|f| f.hash.clone())
        .collect();
    for entry in std::fs::read_dir(&dir)? {
        let p = entry?.path();
        if let Some(name) = p.file_name().and_then(|n| n.to_str())
            && !referenced.contains(name)
        {
            let _ = std::fs::remove_file(&p);
        }
    }
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

/// One step's effect on a traced env var or file, for `stepci why`.
#[derive(Debug, Clone)]
pub struct TraceEntry {
    pub step: usize,
    pub label: String,
    /// What the step did to the name, e.g. `set it to 2.0.0`, `modified it`.
    pub effect: String,
}

/// Every step in a run that touched an env var or file `name`, in order — the
/// provenance/blame timeline (which also reveals multi-writes: a value set twice
/// shows both, and the last one wins).
pub fn trace(run: &RunRecord, name: &str) -> Vec<TraceEntry> {
    let mut out = Vec::new();
    for step in run.jobs.iter().flat_map(|j| &j.steps) {
        let mut push = |effect: String| {
            out.push(TraceEntry {
                step: step.number,
                label: step.label.clone(),
                effect,
            });
        };
        if let Some((_, v)) = step.env_added.iter().find(|(k, _)| k == name) {
            push(format!("set it to {v}"));
        }
        if let Some((_, o, n)) = step.env_changed.iter().find(|(k, _, _)| k == name) {
            push(format!("changed it {o} → {n}"));
        }
        if step.env_removed.iter().any(|k| k == name) {
            push("removed it from the env".to_string());
        }
        if step.path_added.iter().any(|p| p == name) {
            push("prepended it to PATH".to_string());
        }
        for f in step.files.iter().filter(|f| f.path == name) {
            push(match f.status.as_str() {
                "added" => "created the file".to_string(),
                "removed" => "removed the file".to_string(),
                _ => "modified the file".to_string(),
            });
        }
    }
    out
}

/// The human-readable differences between the same step across two runs (empty
/// if the step behaved identically as far as the recording captured).
pub fn step_changes(a: &StepRecord, b: &StepRecord) -> Vec<String> {
    let mut out = Vec::new();
    if a.outcome != b.outcome {
        out.push(format!("outcome: {} → {}", a.outcome, b.outcome));
    }
    if a.exit_code != b.exit_code {
        out.push(format!(
            "exit: {} → {}",
            opt_code(a.exit_code),
            opt_code(b.exit_code)
        ));
    }
    if a.failure != b.failure {
        out.push(format!(
            "failure: {} → {}",
            a.failure.as_deref().unwrap_or("none"),
            b.failure.as_deref().unwrap_or("none")
        ));
    }
    for (k, av, bv) in map_diff(&env_effects(a), &env_effects(b)) {
        out.push(format!("env {k}: {av} → {bv}"));
    }
    // Files: a different status is always a divergence; the same status is one
    // only when both content hashes exist and differ (so "both wrote the same
    // bytes" isn't reported, and a content-preserving change still is).
    let fa = file_index(a);
    let fb = file_index(b);
    let paths: std::collections::BTreeSet<&String> = fa.keys().chain(fb.keys()).collect();
    for p in paths {
        match (fa.get(p), fb.get(p)) {
            (Some((sa, ha)), Some((sb, hb))) => {
                if sa != sb {
                    out.push(format!("file {p}: {sa} → {sb}"));
                } else if let (Some(ha), Some(hb)) = (ha, hb)
                    && ha != hb
                {
                    out.push(format!("file {p}: {sa}, content differs"));
                }
            }
            (Some((sa, _)), None) => out.push(format!("file {p}: {sa} → —")),
            (None, Some((sb, _))) => out.push(format!("file {p}: — → {sb}")),
            (None, None) => {}
        }
    }
    out
}

fn opt_code(c: Option<i32>) -> String {
    c.map(|c| c.to_string()).unwrap_or_else(|| "—".to_string())
}

fn env_effects(s: &StepRecord) -> std::collections::BTreeMap<String, String> {
    let mut m = std::collections::BTreeMap::new();
    for (k, v) in &s.env_added {
        m.insert(k.clone(), format!("set to {v}"));
    }
    for (k, _, n) in &s.env_changed {
        m.insert(k.clone(), format!("changed to {n}"));
    }
    for k in &s.env_removed {
        m.insert(k.clone(), "removed".to_string());
    }
    m
}

type FileIndex = std::collections::BTreeMap<String, (String, Option<String>)>;

fn file_index(s: &StepRecord) -> FileIndex {
    s.files
        .iter()
        .map(|f| (f.path.clone(), (f.status.clone(), f.hash.clone())))
        .collect()
}

/// Keys whose value differs between two maps, with each side's value (or `—`).
fn map_diff(
    a: &std::collections::BTreeMap<String, String>,
    b: &std::collections::BTreeMap<String, String>,
) -> Vec<(String, String, String)> {
    let keys: std::collections::BTreeSet<&String> = a.keys().chain(b.keys()).collect();
    keys.into_iter()
        .filter_map(|k| {
            let av = a.get(k).map(String::as_str).unwrap_or("—");
            let bv = b.get(k).map(String::as_str).unwrap_or("—");
            (av != bv).then(|| (k.clone(), av.to_string(), bv.to_string()))
        })
        .collect()
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

    #[test]
    fn step_changes_reports_outcome_env_and_file_divergences() {
        let fc = |path: &str, status: &str, hash: Option<&str>| FileChange {
            path: path.into(),
            status: status.into(),
            dir_files: None,
            hash: hash.map(String::from),
        };
        let a = StepRecord {
            outcome: "failure".into(),
            exit_code: Some(1),
            env_added: vec![("V".into(), "1".into())],
            files: vec![
                fc("out/app", "modified", Some("aaa")),
                fc("out/log", "modified", Some("h1")),
            ],
            failure: Some("failed at line 2: cp x y (exit 1)".into()),
            ..Default::default()
        };
        let b = StepRecord {
            outcome: "success".into(),
            exit_code: Some(0),
            env_added: vec![("V".into(), "2".into())],
            files: vec![
                fc("out/app", "added", None),
                fc("out/log", "modified", Some("h2")),
            ],
            ..Default::default()
        };
        let d = step_changes(&a, &b);
        assert!(d.iter().any(|l| l == "outcome: failure → success"));
        assert!(d.iter().any(|l| l == "exit: 1 → 0"));
        assert!(d.iter().any(|l| l.starts_with("failure:")));
        assert!(d.iter().any(|l| l == "env V: set to 1 → set to 2"));
        // Different status is always a divergence.
        assert!(d.iter().any(|l| l == "file out/app: modified → added"));
        // Same status but different content hash → "content differs".
        assert!(
            d.iter()
                .any(|l| l == "file out/log: modified, content differs")
        );

        // Identical steps → no changes (same status + same hash isn't a diff).
        assert!(step_changes(&a, &a).is_empty());
    }

    #[test]
    fn trace_follows_an_env_var_and_a_file_across_steps() {
        let step = |number: usize, label: &str, s: StepRecord| StepRecord {
            number,
            label: label.into(),
            ..s
        };
        let run = RunRecord {
            format_version: FORMAT_VERSION,
            stepci_version: "x".into(),
            workflow: "w".into(),
            started_unix_ms: 1,
            exit_code: 0,
            jobs: vec![JobRecord {
                id: "j".into(),
                name: None,
                matrix: String::new(),
                status: "success".into(),
                steps: vec![
                    step(
                        1,
                        "Configure",
                        StepRecord {
                            env_added: vec![("VERSION".into(), "1.0.0".into())],
                            ..Default::default()
                        },
                    ),
                    step(
                        2,
                        "Build",
                        StepRecord {
                            files: vec![FileChange {
                                path: "out/app".into(),
                                status: "added".into(),
                                ..Default::default()
                            }],
                            ..Default::default()
                        },
                    ),
                    step(
                        3,
                        "Bump",
                        StepRecord {
                            env_changed: vec![("VERSION".into(), "1.0.0".into(), "2.0.0".into())],
                            ..Default::default()
                        },
                    ),
                ],
            }],
        };
        // VERSION: two writes, in order.
        let v = trace(&run, "VERSION");
        assert_eq!(v.len(), 2);
        assert_eq!((v[0].step, v[0].effect.as_str()), (1, "set it to 1.0.0"));
        assert_eq!(
            (v[1].step, v[1].effect.as_str()),
            (3, "changed it 1.0.0 → 2.0.0")
        );
        // A file.
        let f = trace(&run, "out/app");
        assert_eq!(f.len(), 1);
        assert_eq!((f[0].step, f[0].effect.as_str()), (2, "created the file"));
        // Nothing.
        assert!(trace(&run, "NOPE").is_empty());
    }
}
