//! The wedge: compute what a step *changed* — the environment it exported (via
//! `$GITHUB_ENV`/`$GITHUB_PATH`) and the files it touched in the workspace.
//!
//! The filesystem diff walks the workspace (skipping `.git`, capped so a giant
//! tree can't hang it) and collapses a wholly-new directory into a single line —
//! so `npm ci` shows `node_modules/ (14203 files)`, not 14k rows.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use indexmap::IndexMap;
use walkdir::WalkDir;

// ---------------------------------------------------------------------------
// Environment diff
// ---------------------------------------------------------------------------

/// What a step changed about the environment carried into later steps.
#[derive(Debug, Default, PartialEq)]
pub struct EnvDiff {
    /// Newly defined variables: `(key, value)`.
    pub added: Vec<(String, String)>,
    /// Redefined variables: `(key, old, new)`.
    pub changed: Vec<(String, String, String)>,
    /// Removed variables (rare — `$GITHUB_ENV` only adds).
    pub removed: Vec<String>,
    /// Directories prepended to `PATH` via `$GITHUB_PATH`.
    pub path_added: Vec<String>,
}

impl EnvDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.changed.is_empty()
            && self.removed.is_empty()
            && self.path_added.is_empty()
    }
}

/// Diff the environment and `PATH` additions between two step boundaries.
pub fn env_diff(
    before: &IndexMap<String, String>,
    after: &IndexMap<String, String>,
    path_before: &[String],
    path_after: &[String],
) -> EnvDiff {
    let mut diff = EnvDiff::default();
    for (k, v) in after {
        match before.get(k) {
            None => diff.added.push((k.clone(), v.clone())),
            Some(old) if old != v => diff.changed.push((k.clone(), old.clone(), v.clone())),
            _ => {}
        }
    }
    for k in before.keys() {
        if !after.contains_key(k) {
            diff.removed.push(k.clone());
        }
    }
    let before_set: HashSet<&String> = path_before.iter().collect();
    diff.path_added = path_after
        .iter()
        .filter(|p| !before_set.contains(*p))
        .cloned()
        .collect();
    diff
}

// ---------------------------------------------------------------------------
// Filesystem diff
// ---------------------------------------------------------------------------

/// A file's change-detection signature. Size + mtime is fast (stat only) and
/// catches essentially every real change; a rewrite preserving both is missed.
#[derive(Debug, Clone, PartialEq)]
struct FileSig {
    size: u64,
    mtime: Option<SystemTime>,
}

/// A snapshot of the workspace's files, keyed by workspace-relative path.
#[derive(Debug)]
pub struct FsSnapshot {
    files: BTreeMap<PathBuf, FileSig>,
    /// Whether the walk hit the file cap and stopped early.
    truncated: bool,
}

/// One entry in a filesystem diff: an individual file, or a collapsed directory.
#[derive(Debug, PartialEq)]
pub enum Entry {
    File(PathBuf),
    Dir(PathBuf, usize),
}

/// A filesystem diff between two snapshots.
#[derive(Debug, Default, PartialEq)]
pub struct FsDiff {
    pub added: Vec<Entry>,
    pub removed: Vec<Entry>,
    pub modified: Vec<PathBuf>,
    pub truncated: bool,
}

impl FsDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.modified.is_empty()
    }
}

/// Snapshot the workspace, skipping `.git` and stopping after `max_files`.
pub fn snapshot_fs(root: &Path, max_files: usize) -> FsSnapshot {
    let mut files = BTreeMap::new();
    let mut truncated = false;

    let walker = WalkDir::new(root)
        .into_iter()
        // Prune the `.git` directory, but not a regular file that happens to be
        // named `.git`.
        .filter_entry(|e| !(e.file_type().is_dir() && e.file_name() == ".git"));
    for entry in walker {
        let Ok(entry) = entry else { continue }; // skip unreadable paths
        if !entry.file_type().is_file() {
            continue;
        }
        if files.len() >= max_files {
            truncated = true;
            break;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let rel = entry
            .path()
            .strip_prefix(root)
            .unwrap_or(entry.path())
            .to_path_buf();
        files.insert(
            rel,
            FileSig {
                size: meta.len(),
                mtime: meta.modified().ok(),
            },
        );
    }
    FsSnapshot { files, truncated }
}

/// Diff two workspace snapshots, collapsing wholly-new/removed directories.
///
/// If either snapshot hit the file cap it only observed a subset of the tree, so
/// a real diff would invent phantom `added`/`removed` entries (files present on
/// disk but evicted from one snapshot). We refuse to guess and return an empty,
/// `truncated` diff instead — the caller reports the workspace was too large.
pub fn fs_diff(before: &FsSnapshot, after: &FsSnapshot) -> FsDiff {
    if before.truncated || after.truncated {
        return FsDiff {
            truncated: true,
            ..FsDiff::default()
        };
    }

    let before_dirs = dirs_of(before);
    let after_dirs = dirs_of(after);

    let mut added_paths = Vec::new();
    let mut modified = Vec::new();
    for (p, sig) in &after.files {
        match before.files.get(p) {
            None => added_paths.push(p.clone()),
            Some(old) if old != sig => modified.push(p.clone()),
            _ => {}
        }
    }
    let removed_paths: Vec<PathBuf> = before
        .files
        .keys()
        .filter(|p| !after.files.contains_key(*p))
        .cloned()
        .collect();

    modified.sort();
    FsDiff {
        added: collapse(&added_paths, &before_dirs),
        removed: collapse(&removed_paths, &after_dirs),
        modified,
        truncated: before.truncated || after.truncated,
    }
}

/// All ancestor directories that contain a file in the snapshot.
fn dirs_of(snap: &FsSnapshot) -> HashSet<PathBuf> {
    let mut set = HashSet::new();
    for p in snap.files.keys() {
        for anc in p.ancestors().skip(1) {
            if anc.as_os_str().is_empty() {
                break;
            }
            set.insert(anc.to_path_buf());
        }
    }
    set
}

/// Collapse a set of changed paths: files under a wholly-new directory are
/// reported as that directory with a count; a lone file is shown as itself.
fn collapse(paths: &[PathBuf], existing_dirs: &HashSet<PathBuf>) -> Vec<Entry> {
    let mut groups: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
    for p in paths {
        groups
            .entry(new_root(p, existing_dirs))
            .or_default()
            .push(p.clone());
    }
    let mut out = Vec::new();
    for (root, members) in groups {
        if members.len() == 1 {
            out.push(Entry::File(members.into_iter().next().unwrap()));
        } else {
            out.push(Entry::Dir(root, members.len()));
        }
    }
    out
}

/// The shallowest ancestor of `path` that didn't already exist — i.e. the root
/// of the newly-created subtree. Falls back to the path itself.
fn new_root(path: &Path, existing_dirs: &HashSet<PathBuf>) -> PathBuf {
    let mut parents: Vec<&Path> = path
        .ancestors()
        .skip(1)
        .filter(|p| !p.as_os_str().is_empty())
        .collect();
    parents.reverse(); // shallowest first
    for d in parents {
        if !existing_dirs.contains(d) {
            return d.to_path_buf();
        }
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn map(pairs: &[(&str, &str)]) -> IndexMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn env_diff_added_changed_removed_and_path() {
        let before = map(&[("A", "1"), ("B", "2")]);
        let after = map(&[("A", "1"), ("B", "changed"), ("C", "new")]);
        let d = env_diff(
            &before,
            &after,
            &["/old".into()],
            &["/new".into(), "/old".into()],
        );
        assert_eq!(d.added, vec![("C".into(), "new".into())]);
        assert_eq!(d.changed, vec![("B".into(), "2".into(), "changed".into())]);
        assert_eq!(d.removed, Vec::<String>::new());
        assert_eq!(d.path_added, vec!["/new".to_string()]);
    }

    fn snap(root: &Path) -> FsSnapshot {
        snapshot_fs(root, 100_000)
    }

    #[test]
    fn fs_diff_collapses_new_directory_and_flags_modified() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("keep.txt"), "hello").unwrap();
        let before = snap(root);

        // Create a wholly-new directory with several files, and modify keep.txt.
        fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        fs::write(root.join("node_modules/a.js"), "a").unwrap();
        fs::write(root.join("node_modules/pkg/b.js"), "b").unwrap();
        fs::write(root.join("node_modules/pkg/c.js"), "c").unwrap();
        fs::write(root.join("keep.txt"), "hello world changed").unwrap();
        let after = snap(root);

        let d = fs_diff(&before, &after);
        // The entire node_modules tree collapses to one directory entry (3 files).
        assert_eq!(d.added, vec![Entry::Dir(PathBuf::from("node_modules"), 3)]);
        assert_eq!(d.modified, vec![PathBuf::from("keep.txt")]);
        assert!(d.removed.is_empty());
    }

    #[test]
    fn fs_diff_lone_new_file_is_not_collapsed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "a").unwrap();
        let before = snap(root);
        fs::write(root.join("b.txt"), "b").unwrap();
        let after = snap(root);

        let d = fs_diff(&before, &after);
        assert_eq!(d.added, vec![Entry::File(PathBuf::from("b.txt"))]);
    }

    #[test]
    fn snapshot_skips_git_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join(".git/config"), "x").unwrap();
        fs::write(root.join("real.txt"), "y").unwrap();
        let snap = snapshot_fs(root, 100_000);
        assert!(snap.files.contains_key(Path::new("real.txt")));
        assert!(!snap.files.keys().any(|p| p.starts_with(".git")));
    }

    #[test]
    fn snapshot_truncates_at_cap() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for i in 0..10 {
            fs::write(root.join(format!("f{i}.txt")), "x").unwrap();
        }
        let snap = snapshot_fs(root, 5);
        assert!(snap.truncated);
        assert_eq!(snap.files.len(), 5);
    }

    #[test]
    fn truncated_snapshots_do_not_invent_changes() {
        // Both snapshots hit the cap and see different subsets — the diff must
        // NOT report phantom added/removed for files that never actually changed.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for i in 0..30 {
            fs::write(root.join(format!("f{i:03}.txt")), "x").unwrap();
        }
        let before = snapshot_fs(root, 20);
        for i in 30..60 {
            fs::write(root.join(format!("f{i:03}.txt")), "x").unwrap();
        }
        let after = snapshot_fs(root, 20);

        let d = fs_diff(&before, &after);
        assert!(d.truncated);
        assert!(
            d.added.is_empty() && d.removed.is_empty() && d.modified.is_empty(),
            "a truncated diff must not invent entries: {d:?}"
        );
    }

    #[test]
    fn regular_file_named_dotgit_is_not_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join(".git"), "i am a file, not a dir").unwrap();
        let snap = snapshot_fs(root, 100);
        assert!(snap.files.contains_key(Path::new(".git")));
    }
}
