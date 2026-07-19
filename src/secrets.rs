//! Load the secrets a run can reference via `${{ secrets.NAME }}`.
//!
//! Sources: a dotenv-style `--secret-file` and/or repeated `--secret NAME[=VALUE]`
//! flags (a bare `NAME` reads the value from the environment). A value of the
//! form `op://…` is resolved through the 1Password CLI and `vault://path#field`
//! through the Vault CLI — so real secrets never have to be written to disk.

use anyhow::{Context, Result, bail};
use indexmap::IndexMap;
use std::path::Path;
use std::process::Command;

/// Load and resolve secrets. Inline `--secret` entries override the file.
pub fn load_secrets(file: Option<&Path>, inline: &[String]) -> Result<IndexMap<String, String>> {
    let mut out = IndexMap::new();
    if let Some(path) = file {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("reading secret file `{}`", path.display()))?;
        for (k, raw) in parse_dotenv(&content)? {
            let value = resolve(&raw).with_context(|| format!("secret `{k}`"))?;
            out.insert(k, value);
        }
    }
    for entry in inline {
        let (k, raw) = split_secret(entry)?;
        let value = resolve(&raw).with_context(|| format!("secret `{k}`"))?;
        out.insert(k, value);
    }
    Ok(out)
}

/// Split a `--secret` entry: `NAME=VALUE`, or bare `NAME` (value from the env).
fn split_secret(entry: &str) -> Result<(String, String)> {
    match entry.split_once('=') {
        Some((k, v)) => {
            let k = k.trim();
            if k.is_empty() {
                bail!("--secret `{entry}`: empty name");
            }
            Ok((k.to_string(), v.to_string()))
        }
        None => {
            let name = entry.trim();
            if name.is_empty() {
                bail!("--secret: empty name");
            }
            let val = std::env::var(name).map_err(|_| {
                anyhow::anyhow!("--secret {name}: environment variable `{name}` is not set")
            })?;
            Ok((name.to_string(), val))
        }
    }
}

/// Resolve a secret value: `op://…` via 1Password, `vault://path#field` via
/// Vault, otherwise the literal value.
fn resolve(raw: &str) -> Result<String> {
    if raw.starts_with("op://") {
        return run_tool("op", &["read", "--no-newline", raw])
            .context("resolving a 1Password (op://) reference");
    }
    if let Some(rest) = raw.strip_prefix("vault://") {
        let (path, field) = rest.rsplit_once('#').ok_or_else(|| {
            anyhow::anyhow!("Vault reference must be `vault://<path>#<field>`, got `{raw}`")
        })?;
        // `--` stops a `path` that starts with `-` from being read as a flag.
        return run_tool(
            "vault",
            &["kv", "get", &format!("-field={field}"), "--", path],
        )
        .context("resolving a Vault (vault://) reference");
    }
    Ok(raw.to_string())
}

/// Run a secret-manager CLI and return its trimmed stdout.
fn run_tool(bin: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(bin).args(args).output().map_err(|e| {
        anyhow::anyhow!("failed to run `{bin}` (is it installed and signed in?): {e}")
    })?;
    if !out.status.success() {
        bail!(
            "`{bin}` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .trim_end_matches(['\n', '\r'])
        .to_string())
}

/// Minimal dotenv parser: `KEY=VALUE`, whole-line `#` comments, blank lines,
/// optional surrounding quotes, optional `export ` prefix.
///
/// There are no *inline* comments — the entire value after `=` is the secret
/// (a trailing `# ...` is kept), since `#` can legitimately appear in a secret.
fn parse_dotenv(content: &str) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for (i, raw) in content.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let (k, v) = line
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("secret file line {}: expected `KEY=VALUE`", i + 1))?;
        let k = k.trim();
        if k.is_empty() {
            bail!("secret file line {}: empty key", i + 1);
        }
        out.push((k.to_string(), unquote(v.trim())));
    }
    Ok(out)
}

/// Strip a single pair of matching surrounding quotes.
fn unquote(s: &str) -> String {
    let bytes = s.as_bytes();
    if bytes.len() >= 2 {
        let (first, last) = (bytes[0], bytes[bytes.len() - 1]);
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dotenv_handles_comments_quotes_and_export() {
        let kv = parse_dotenv(
            "# a comment\nexport TOKEN=abc123\nQUOTED=\"a b c\"\nSINGLE='x y'\n\nEMPTY=\n",
        )
        .unwrap();
        assert_eq!(
            kv,
            vec![
                ("TOKEN".into(), "abc123".into()),
                ("QUOTED".into(), "a b c".into()),
                ("SINGLE".into(), "x y".into()),
                ("EMPTY".into(), "".into()),
            ]
        );
    }

    #[test]
    fn dotenv_rejects_a_line_without_equals() {
        assert!(parse_dotenv("VALID=1\nnonsense\n").is_err());
    }

    #[test]
    fn split_inline_value_and_env_lookup() {
        assert_eq!(
            split_secret("A=b=c").unwrap(),
            ("A".to_string(), "b=c".to_string())
        );
        // Bare name reads from the environment.
        // SAFETY: single-threaded within this test; unique var name.
        unsafe { std::env::set_var("STEPCI_TEST_SECRET_UNIQUE", "sekret") };
        assert_eq!(
            split_secret("STEPCI_TEST_SECRET_UNIQUE").unwrap(),
            (
                "STEPCI_TEST_SECRET_UNIQUE".to_string(),
                "sekret".to_string()
            )
        );
        assert!(split_secret("STEPCI_NOT_SET_ANYWHERE_XYZ").is_err());
    }

    #[test]
    fn resolve_passes_through_literal_values() {
        assert_eq!(resolve("plain-value").unwrap(), "plain-value");
    }

    #[test]
    fn vault_reference_requires_a_field() {
        // Missing `#field` is rejected before we ever shell out.
        assert!(resolve("vault://secret/data/app").is_err());
    }
}
