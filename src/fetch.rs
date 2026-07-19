//! Fetch remote `owner/repo[/subdir]@ref` actions into a local cache via git,
//! so they can be resolved and run like local ones.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

/// A parsed remote action reference.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteRef {
    pub owner: String,
    pub repo: String,
    /// Path to the action within the repo, if it isn't at the root.
    pub subpath: Option<String>,
    /// The git ref: a tag, branch, or commit SHA.
    pub git_ref: String,
}

/// Parse an `owner/repo[/sub/path]@ref` reference. Returns `None` if it isn't
/// that shape (e.g. a local `./…` path) or if any component is unsafe.
///
/// Components are validated to prevent git-argument injection (a `ref` like
/// `--upload-pack=…` would otherwise be a flag) and cache-path traversal (`..`).
pub fn parse_remote(reference: &str) -> Option<RemoteRef> {
    let (path, git_ref) = reference.split_once('@')?;
    let mut parts = path.splitn(3, '/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    let subpath = parts.next().map(str::to_string).filter(|s| !s.is_empty());

    if !valid_name(owner) || !valid_name(repo) || !valid_ref(git_ref) {
        return None;
    }
    if let Some(sub) = &subpath
        && !valid_subpath(sub)
    {
        return None;
    }
    Some(RemoteRef {
        owner: owner.to_string(),
        repo: repo.to_string(),
        subpath,
        git_ref: git_ref.to_string(),
    })
}

/// An owner/repo name: alphanumerics plus `.`/`-`/`_`, no leading `-`, no `..`.
fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('-')
        && !s.contains("..")
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

/// A git ref: like a name but may contain `/` (branch/tag paths). No leading `-`.
fn valid_ref(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('-')
        && !s.contains("..")
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '/'))
}

/// A sub-path within the repo: same as a ref but must not escape the repo.
fn valid_subpath(s: &str) -> bool {
    valid_ref(s) && !s.starts_with('/')
}

/// The root of the on-disk action cache (`~/.cache/stepci/actions`).
pub fn cache_root() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .ok_or_else(|| anyhow::anyhow!("no HOME directory for the action cache"))?;
    Ok(PathBuf::from(home).join(".cache/stepci/actions"))
}

/// Fetch the action's repo at its ref into the cache (reusing a prior fetch) and
/// return the directory containing its `action.yml`.
pub fn fetch(r: &RemoteRef, cache_root: &Path) -> Result<PathBuf> {
    let safe_ref = r.git_ref.replace(['/', '\\'], "_");
    let repo_dir = cache_root.join(&r.owner).join(&r.repo).join(&safe_ref);

    if !repo_dir.join(".git").is_dir() {
        std::fs::create_dir_all(&repo_dir)
            .with_context(|| format!("creating cache dir `{}`", repo_dir.display()))?;
        let url = format!("https://github.com/{}/{}", r.owner, r.repo);
        // Shallow-fetch the exact ref (works for a tag, branch, or SHA).
        run_git(&repo_dir, &["init", "-q"])?;
        run_git(&repo_dir, &["remote", "add", "origin", &url])?;
        run_git(
            &repo_dir,
            &["fetch", "-q", "--depth", "1", "origin", &r.git_ref],
        )
        .with_context(|| format!("fetching `{}/{}@{}`", r.owner, r.repo, r.git_ref))?;
        run_git(&repo_dir, &["checkout", "-q", "FETCH_HEAD"])?;
    }

    Ok(match &r.subpath {
        Some(sub) => repo_dir.join(sub),
        None => repo_dir,
    })
}

fn run_git(dir: &Path, args: &[&str]) -> Result<()> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| anyhow::anyhow!("failed to run git: {e}"))?;
    if !out.status.success() {
        bail!(
            "git {}: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_remote_references() {
        assert_eq!(
            parse_remote("actions/checkout@v4"),
            Some(RemoteRef {
                owner: "actions".into(),
                repo: "checkout".into(),
                subpath: None,
                git_ref: "v4".into(),
            })
        );
        assert_eq!(
            parse_remote("owner/repo/path/to/action@abc123"),
            Some(RemoteRef {
                owner: "owner".into(),
                repo: "repo".into(),
                subpath: Some("path/to/action".into()),
                git_ref: "abc123".into(),
            })
        );
    }

    #[test]
    fn rejects_non_remote_references() {
        assert_eq!(parse_remote("./local/action"), None); // no `@`
        assert_eq!(parse_remote("actions/checkout"), None); // no ref
        assert_eq!(parse_remote("noslash@v1"), None); // no repo
        assert_eq!(parse_remote("owner/repo@"), None); // empty ref
    }

    #[test]
    fn rejects_unsafe_components() {
        // Git-arg injection: a ref that would be read as a flag.
        assert_eq!(parse_remote("owner/repo@--upload-pack=touch /tmp/x"), None);
        // Path traversal via `..` in any component.
        assert_eq!(parse_remote("../evil/repo@v1"), None);
        assert_eq!(parse_remote("owner/repo@../../etc"), None);
        assert_eq!(parse_remote("owner/repo/..%2f@v1"), None);
        // A normal branch-path ref is still fine.
        assert!(parse_remote("owner/repo@refs/tags/v1.2.3").is_some());
    }
}
