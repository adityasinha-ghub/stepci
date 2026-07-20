//! Local backing store for the `actions/cache` shim.
//!
//! GitHub's cache actions talk to a hosted cache service; stepci instead stores
//! cache entries under `~/.cache/stepci/cache`, keyed by the workflow's cache
//! `key`, so a key hit on a later local run restores the files. Only the pure
//! path/key logic lives here; the copying and prefix search are in `exec`.

use anyhow::{Result, anyhow};
use std::path::{Path, PathBuf};

/// The root of the on-disk cache (`~/.cache/stepci/cache`).
pub fn cache_root() -> Result<PathBuf> {
    Ok(home()?.join(".cache/stepci/cache"))
}

/// The directory that holds the entry for a given cache key.
pub fn entry_dir(root: &Path, key: &str) -> PathBuf {
    root.join(hash_key(key))
}

/// A stable, filesystem-safe directory name for a cache key (its hash). Stable
/// across runs so a later run finds the same entry.
fn hash_key(key: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h);
    format!("{:x}", h.finish())
}

/// Resolve a cache `path` entry to an absolute path: a leading `~/` expands to
/// the home directory, and a relative path is taken under the workspace.
pub fn resolve_path(entry: &str, workspace: &Path) -> PathBuf {
    let entry = entry.trim();
    if entry == "~"
        && let Ok(h) = home()
    {
        return h;
    }
    if let Some(rest) = entry.strip_prefix("~/")
        && let Ok(h) = home()
    {
        return h.join(rest);
    }
    let p = Path::new(entry);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        workspace.join(p)
    }
}

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("no HOME directory for the cache"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_hash_is_stable_and_distinct() {
        assert_eq!(
            entry_dir(Path::new("/c"), "k"),
            entry_dir(Path::new("/c"), "k")
        );
        assert_ne!(
            entry_dir(Path::new("/c"), "deps-abc"),
            entry_dir(Path::new("/c"), "deps-xyz")
        );
    }

    #[test]
    fn resolve_relative_is_under_workspace() {
        let ws = Path::new("/ws");
        assert_eq!(
            resolve_path("node_modules", ws),
            PathBuf::from("/ws/node_modules")
        );
        assert_eq!(resolve_path("/abs/path", ws), PathBuf::from("/abs/path"));
    }

    #[test]
    fn resolve_tilde_expands_to_home() {
        // SAFETY: single-threaded test; we set and read HOME immediately.
        unsafe { std::env::set_var("HOME", "/home/tester") };
        assert_eq!(
            resolve_path("~/.npm", Path::new("/ws")),
            PathBuf::from("/home/tester/.npm")
        );
        assert_eq!(
            resolve_path("~", Path::new("/ws")),
            PathBuf::from("/home/tester")
        );
    }
}
