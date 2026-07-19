//! Parse the `::workflow-command::` lines that steps and actions print to their
//! stdout — the runner's other back-channel besides the `$GITHUB_*` files.
//!
//! The line shape is `::name key=val,key2=val2::message`, where the parameter
//! block and the message use percent-escaping (`%25`→`%`, `%0A`→`\n`, `%0D`→`\r`,
//! and additionally `%3A`→`:`, `%2C`→`,` inside parameter values). This module is
//! pure text handling — hence its own tests.

/// A parsed workflow command: its name, its parameters, and its message, all
/// percent-decoded. The executor decides what each name means.
#[derive(Debug, Clone, PartialEq)]
pub struct Command {
    /// The command name, e.g. `set-output`, `add-mask`, `warning`.
    pub name: String,
    /// Decoded `key=value` parameters from the block before the message.
    pub params: Vec<(String, String)>,
    /// The decoded message (everything after the second `::`).
    pub message: String,
}

impl Command {
    /// The value of a named parameter, if present.
    pub fn param(&self, key: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

/// Parse one line as a workflow command, or `None` if it isn't one.
///
/// A command needs the `::name…::` shape with a closing `::`; a bare `::foo`
/// (e.g. a C++ scope in ordinary output) is not a command and returns `None`.
pub fn parse(line: &str) -> Option<Command> {
    let rest = line.strip_prefix("::")?;
    // The first `::` after the head ends the name+params block. Parameter values
    // encode a literal `:` as `%3A`, so the first real `::` is unambiguous.
    let sep = rest.find("::")?;
    let head = &rest[..sep];
    let message = &rest[sep + 2..];

    let (name, param_str) = match head.split_once(' ') {
        Some((n, p)) => (n, p),
        None => (head, ""),
    };
    if name.is_empty() {
        return None;
    }

    let mut params = Vec::new();
    if !param_str.is_empty() {
        for kv in param_str.split(',') {
            if let Some((k, v)) = kv.split_once('=') {
                params.push((k.trim().to_string(), decode_property(v)));
            }
        }
    }

    Some(Command {
        name: name.to_string(),
        params,
        message: decode_message(message),
    })
}

/// Decode a command message. `%25` is decoded last so an already-literal escape
/// like `%0A` in the source round-trips (it was encoded as `%250A`).
fn decode_message(s: &str) -> String {
    s.replace("%0D", "\r")
        .replace("%0A", "\n")
        .replace("%25", "%")
}

/// Decode a parameter value: like a message, but `:` and `,` are also escaped.
fn decode_property(s: &str) -> String {
    s.replace("%0D", "\r")
        .replace("%0A", "\n")
        .replace("%3A", ":")
        .replace("%2C", ",")
        .replace("%25", "%")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_output_with_name_param() {
        let c = parse("::set-output name=result::hello world").unwrap();
        assert_eq!(c.name, "set-output");
        assert_eq!(c.param("name"), Some("result"));
        assert_eq!(c.message, "hello world");
    }

    #[test]
    fn add_mask_has_no_params() {
        let c = parse("::add-mask::s3cr3t").unwrap();
        assert_eq!(c.name, "add-mask");
        assert!(c.params.is_empty());
        assert_eq!(c.message, "s3cr3t");
    }

    #[test]
    fn endgroup_has_empty_message() {
        let c = parse("::endgroup::").unwrap();
        assert_eq!(c.name, "endgroup");
        assert_eq!(c.message, "");
    }

    #[test]
    fn message_may_contain_double_colons() {
        let c = parse("::set-output name=x::a::b::c").unwrap();
        assert_eq!(c.param("name"), Some("x"));
        assert_eq!(c.message, "a::b::c");
    }

    #[test]
    fn percent_escapes_are_decoded() {
        // A newline in the value, and a literal `%` and `:` in a property.
        let c = parse("::set-output name=a%3Ab::line1%0Aline2%25done").unwrap();
        assert_eq!(c.param("name"), Some("a:b"));
        assert_eq!(c.message, "line1\nline2%done");
    }

    #[test]
    fn multiple_params_and_file_line() {
        let c = parse("::error file=app.js,line=10,col=5::Something broke").unwrap();
        assert_eq!(c.name, "error");
        assert_eq!(c.param("file"), Some("app.js"));
        assert_eq!(c.param("line"), Some("10"));
        assert_eq!(c.param("col"), Some("5"));
        assert_eq!(c.message, "Something broke");
    }

    #[test]
    fn non_commands_return_none() {
        assert!(parse("regular output").is_none());
        assert!(parse("std::vector<int> v;").is_none()); // no leading `::`
        assert!(parse("::just-a-prefix").is_none()); // no closing `::`
        assert!(parse("::::").is_none()); // empty command name
        assert!(parse("::debug::").is_some()); // minimal well-formed command
    }
}
