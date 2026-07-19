//! Native shims for `actions/upload-artifact` and `actions/download-artifact`.
//!
//! GitHub's artifact actions talk to a hosted artifact service (an authenticated
//! HTTP/Twirp API). Locally there's no such service, and for a debugger you don't
//! want real uploads anyway — so stepci recognizes these actions by name and
//! backs them with a run-local directory, letting artifacts pass between jobs
//! offline. Only the pure path logic lives here; the copying is in `exec`.

use std::path::{Component, Path, PathBuf};

/// Which artifact action a `uses:` reference names, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Upload,
    Download,
}

/// Recognize `actions/upload-artifact` / `actions/download-artifact` at any
/// version (`@v4`, `@main`, a SHA, …), so the shim backs them locally.
pub fn kind(reference: &str) -> Option<Kind> {
    match reference.split('@').next().unwrap_or(reference) {
        "actions/upload-artifact" => Some(Kind::Upload),
        "actions/download-artifact" => Some(Kind::Download),
        _ => None,
    }
}

/// The longest common directory prefix of a set of files. GitHub strips this
/// from uploaded artifact paths, so `path: dist` stores `dist`'s *contents* (not
/// a nested `dist/` folder); download then restores them under the target path.
pub fn common_dir_prefix(files: &[PathBuf]) -> PathBuf {
    let mut iter = files.iter();
    let Some(first) = iter.next() else {
        return PathBuf::new();
    };
    let mut prefix: Vec<Component> = first
        .parent()
        .unwrap_or(Path::new(""))
        .components()
        .collect();
    for f in iter {
        let comps: Vec<Component> = f.parent().unwrap_or(Path::new("")).components().collect();
        let common = prefix
            .iter()
            .zip(&comps)
            .take_while(|(a, b)| a == b)
            .count();
        prefix.truncate(common);
    }
    prefix.iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_artifact_actions_at_any_version() {
        assert_eq!(kind("actions/upload-artifact@v4"), Some(Kind::Upload));
        assert_eq!(kind("actions/download-artifact@main"), Some(Kind::Download));
        assert_eq!(kind("actions/upload-artifact"), Some(Kind::Upload));
        assert_eq!(kind("actions/checkout@v4"), None);
        assert_eq!(kind("./local"), None);
    }

    #[test]
    fn common_prefix_of_a_single_file_is_its_parent() {
        let files = vec![PathBuf::from("/ws/dist/app.js")];
        assert_eq!(common_dir_prefix(&files), PathBuf::from("/ws/dist"));
    }

    #[test]
    fn common_prefix_strips_shared_directory() {
        let files = vec![
            PathBuf::from("/ws/dist/app.js"),
            PathBuf::from("/ws/dist/sub/b.js"),
        ];
        // `dist` is shared, so its contents (app.js, sub/b.js) are what's stored.
        assert_eq!(common_dir_prefix(&files), PathBuf::from("/ws/dist"));
    }

    #[test]
    fn common_prefix_of_disjoint_trees_is_their_ancestor() {
        let files = vec![
            PathBuf::from("/ws/dist/app.js"),
            PathBuf::from("/ws/build/b.js"),
        ];
        assert_eq!(common_dir_prefix(&files), PathBuf::from("/ws"));
    }

    #[test]
    fn common_prefix_of_empty_is_empty() {
        assert_eq!(common_dir_prefix(&[]), PathBuf::new());
    }
}
