//! Parse the files a step writes to talk back to the runner: `$GITHUB_ENV` and
//! `$GITHUB_OUTPUT` (same `KEY=VALUE`-or-heredoc format) and `$GITHUB_PATH`
//! (one path per line). This is pure, fiddly text handling — hence its own tests.

use anyhow::{Result, bail};

/// Parse `$GITHUB_ENV` / `$GITHUB_OUTPUT` content into ordered key/value pairs.
///
/// Two line forms are supported, matching the runner:
/// - `KEY=VALUE`
/// - a heredoc, for multi-line values:
///   ```text
///   KEY<<DELIM
///   line one
///   line two
///   DELIM
///   ```
///
/// When a line has both `=` and `<<`, whichever appears first decides the form
/// (so `EXPR=a<<b` is a `KEY=VALUE` assignment, not a heredoc).
pub fn parse_key_values(content: &str) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    let mut lines = content.lines();
    while let Some(line) = lines.next() {
        if line.trim().is_empty() {
            continue;
        }
        let lt = line.find("<<");
        let eq = line.find('=');
        let is_heredoc = match (lt, eq) {
            (Some(l), Some(e)) => l < e,
            (Some(_), None) => true,
            _ => false,
        };

        if is_heredoc {
            let l = lt.expect("is_heredoc implies `<<` is present");
            let key = line[..l].trim().to_string();
            let delim = line[l + 2..].trim().to_string();
            if key.is_empty() || delim.is_empty() {
                bail!("invalid heredoc line: `{line}`");
            }
            let mut value = Vec::new();
            let mut closed = false;
            for vline in lines.by_ref() {
                if vline == delim {
                    closed = true;
                    break;
                }
                value.push(vline);
            }
            if !closed {
                bail!("unterminated heredoc for `{key}` (missing closing `{delim}`)");
            }
            out.push((key, value.join("\n")));
        } else if let Some(e) = eq {
            let key = line[..e].trim().to_string();
            if key.is_empty() {
                bail!("invalid line (empty key): `{line}`");
            }
            out.push((key, line[e + 1..].to_string()));
        } else {
            bail!("invalid line in env file (expected `KEY=VALUE` or a heredoc): `{line}`");
        }
    }
    Ok(out)
}

/// Parse `$GITHUB_PATH`: each non-blank line is a directory to prepend to `PATH`.
pub fn parse_path_additions(content: &str) -> Vec<String> {
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_assignments() {
        let kv = parse_key_values("A=1\nB=hello world\n\nC=\n").unwrap();
        assert_eq!(
            kv,
            vec![
                ("A".into(), "1".into()),
                ("B".into(), "hello world".into()),
                ("C".into(), "".into()),
            ]
        );
    }

    #[test]
    fn value_may_contain_equals() {
        let kv = parse_key_values("QUERY=a=b=c").unwrap();
        assert_eq!(kv, vec![("QUERY".into(), "a=b=c".into())]);
    }

    #[test]
    fn heredoc_multiline() {
        let kv = parse_key_values("JSON<<EOF\n{\n  \"x\": 1\n}\nEOF\nNEXT=ok\n").unwrap();
        assert_eq!(kv[0], ("JSON".into(), "{\n  \"x\": 1\n}".into()));
        assert_eq!(kv[1], ("NEXT".into(), "ok".into()));
    }

    #[test]
    fn equals_before_shift_is_an_assignment_not_heredoc() {
        let kv = parse_key_values("EXPR=a<<b").unwrap();
        assert_eq!(kv, vec![("EXPR".into(), "a<<b".into())]);
    }

    #[test]
    fn unterminated_heredoc_errors() {
        let err = parse_key_values("K<<END\nline\n").unwrap_err();
        assert!(format!("{err:#}").contains("unterminated heredoc"));
    }

    #[test]
    fn path_additions_skip_blank_lines() {
        assert_eq!(
            parse_path_additions("/a/bin\n\n/b/bin\n"),
            vec!["/a/bin".to_string(), "/b/bin".to_string()]
        );
    }
}
