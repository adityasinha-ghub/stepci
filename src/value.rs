//! The dynamic value type produced by evaluating `${{ }}` expressions, plus the
//! GitHub Actions coercion rules (to-number, to-string, truthiness) that the
//! evaluator relies on.
//!
//! These rules are deliberately faithful to GitHub's documented behavior — loose
//! numeric coercion for comparisons, `''`→0, hex parsing, case-insensitive string
//! logic — because getting them subtly wrong is exactly how an `if:` condition
//! silently evaluates the opposite of what the real runner would do.

use indexmap::IndexMap;

/// A value in the expression language.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Number(f64),
    Str(String),
    Array(Vec<Value>),
    Object(IndexMap<String, Value>),
}

impl Value {
    /// Coerce to a number following GitHub's rules (used by comparisons).
    ///
    /// `null`→0, `true`→1, `false`→0, strings parsed (empty→0, `0x..` hex),
    /// arrays/objects→NaN.
    pub fn to_number(&self) -> f64 {
        match self {
            Value::Null => 0.0,
            Value::Bool(true) => 1.0,
            Value::Bool(false) => 0.0,
            Value::Number(n) => *n,
            Value::Str(s) => parse_number(s),
            Value::Array(_) | Value::Object(_) => f64::NAN,
        }
    }

    /// Coerce to a string for interpolation / `format` / `join`.
    ///
    /// Scalars convert as GitHub does; arrays/objects render as compact JSON,
    /// which is more useful for a debugger than GitHub's `"Object"` placeholder.
    pub fn to_display_string(&self) -> String {
        match self {
            Value::Null => String::new(),
            Value::Bool(b) => b.to_string(),
            Value::Number(n) => format_number(*n),
            Value::Str(s) => s.clone(),
            Value::Array(_) | Value::Object(_) => {
                serde_json::to_string(&self.to_json()).unwrap_or_default()
            }
        }
    }

    /// Truthiness, as used by `!`, `&&`, `||`, and `if:`.
    ///
    /// `null`→false, numbers→false only for 0/NaN, strings→false only when empty,
    /// arrays/objects→always true.
    pub fn is_truthy(&self) -> bool {
        match self {
            Value::Null => false,
            Value::Bool(b) => *b,
            Value::Number(n) => *n != 0.0 && !n.is_nan(),
            Value::Str(s) => !s.is_empty(),
            Value::Array(_) | Value::Object(_) => true,
        }
    }

    /// Convert to a `serde_json::Value` (for `toJSON`). Integral numbers are
    /// emitted without a decimal point to match GitHub's output.
    pub fn to_json(&self) -> serde_json::Value {
        use serde_json::Value as J;
        match self {
            Value::Null => J::Null,
            Value::Bool(b) => J::Bool(*b),
            Value::Number(n) => number_to_json(*n),
            Value::Str(s) => J::String(s.clone()),
            Value::Array(a) => J::Array(a.iter().map(Value::to_json).collect()),
            Value::Object(o) => {
                let mut map = serde_json::Map::new();
                for (k, v) in o {
                    map.insert(k.clone(), v.to_json());
                }
                J::Object(map)
            }
        }
    }

    /// Convert from a `serde_json::Value` (for `fromJSON`).
    pub fn from_json(v: serde_json::Value) -> Value {
        use serde_json::Value as J;
        match v {
            J::Null => Value::Null,
            J::Bool(b) => Value::Bool(b),
            J::Number(n) => Value::Number(n.as_f64().unwrap_or(f64::NAN)),
            J::String(s) => Value::Str(s),
            J::Array(a) => Value::Array(a.into_iter().map(Value::from_json).collect()),
            J::Object(o) => Value::Object(
                o.into_iter()
                    .map(|(k, v)| (k, Value::from_json(v)))
                    .collect(),
            ),
        }
    }
}

/// Parse a string to a number the way the runner's `ParseNumber` does: trimmed,
/// empty→0, `0x` hex, `0o` octal, the exact spellings `Infinity`/`-Infinity`, and
/// otherwise a decimal/exponent float. Anything else — including Rust's `inf`,
/// `infinity`, and `nan` spellings, which GitHub does *not* accept — is NaN.
fn parse_number(s: &str) -> f64 {
    let t = s.trim();
    if t.is_empty() {
        return 0.0;
    }
    match t {
        "Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        return i64::from_str_radix(hex, 16)
            .map(|i| i as f64)
            .unwrap_or(f64::NAN);
    }
    if let Some(oct) = t.strip_prefix("0o").or_else(|| t.strip_prefix("0O")) {
        return i64::from_str_radix(oct, 8)
            .map(|i| i as f64)
            .unwrap_or(f64::NAN);
    }
    // Reject the float specials Rust's parser would otherwise accept.
    if matches!(
        t.to_ascii_lowercase().as_str(),
        "inf" | "+inf" | "-inf" | "infinity" | "+infinity" | "-infinity" | "nan" | "+nan" | "-nan"
    ) {
        return f64::NAN;
    }
    t.parse::<f64>().unwrap_or(f64::NAN)
}

/// Format a number for string coercion: integers print without a trailing `.0`.
/// This approximates GitHub's `G15` formatting; it can differ for values needing
/// more than 15 significant digits or exponent notation (rare — the language has
/// no arithmetic, so such values only arrive via `fromJSON`/contexts). See README.
fn format_number(n: f64) -> String {
    if n.is_nan() {
        "NaN".to_string()
    } else if n.is_infinite() {
        if n > 0.0 { "Infinity" } else { "-Infinity" }.to_string()
    } else if n == n.trunc() && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        n.to_string()
    }
}

/// Build a JSON number, preferring an integer representation when the value is
/// integral so `toJSON(1)` is `1`, not `1.0`.
fn number_to_json(n: f64) -> serde_json::Value {
    use serde_json::Value as J;
    if n.is_finite() && n == n.trunc() && n.abs() < 1e15 {
        J::Number((n as i64).into())
    } else {
        serde_json::Number::from_f64(n)
            .map(J::Number)
            .unwrap_or(J::Null)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_number_coercions() {
        assert_eq!(Value::Null.to_number(), 0.0);
        assert_eq!(Value::Bool(true).to_number(), 1.0);
        assert_eq!(Value::Bool(false).to_number(), 0.0);
        assert_eq!(Value::Str("".into()).to_number(), 0.0);
        assert_eq!(Value::Str("  42 ".into()).to_number(), 42.0);
        assert_eq!(Value::Str("0xff".into()).to_number(), 255.0);
        assert!(Value::Str("nope".into()).to_number().is_nan());
        assert!(Value::Array(vec![]).to_number().is_nan());
    }

    #[test]
    fn number_coercion_matches_github_specials() {
        // Exact `Infinity` parses; Rust's `inf`/`infinity`/`nan` spellings do not.
        assert_eq!(Value::Str("Infinity".into()).to_number(), f64::INFINITY);
        assert_eq!(
            Value::Str("-Infinity".into()).to_number(),
            f64::NEG_INFINITY
        );
        assert!(Value::Str("inf".into()).to_number().is_nan());
        assert!(Value::Str("infinity".into()).to_number().is_nan());
        assert!(Value::Str("nan".into()).to_number().is_nan());
        // Octal, like the runner.
        assert_eq!(Value::Str("0o17".into()).to_number(), 15.0);
    }

    #[test]
    fn truthiness() {
        assert!(!Value::Null.is_truthy());
        assert!(!Value::Number(0.0).is_truthy());
        assert!(!Value::Number(f64::NAN).is_truthy());
        assert!(Value::Number(3.0).is_truthy());
        assert!(!Value::Str("".into()).is_truthy());
        assert!(Value::Str("x".into()).is_truthy());
        assert!(Value::Array(vec![]).is_truthy());
    }

    #[test]
    fn integral_numbers_stringify_without_decimals() {
        assert_eq!(Value::Number(1.0).to_display_string(), "1");
        assert_eq!(Value::Number(255.0).to_display_string(), "255");
        assert_eq!(Value::Number(1.5).to_display_string(), "1.5");
        assert_eq!(Value::Null.to_display_string(), "");
    }

    #[test]
    fn json_round_trip_preserves_shape_and_order() {
        let v = Value::from_json(serde_json::json!({"b": 1, "a": [true, "x"]}));
        // Object key order preserved (serde_json preserve_order + IndexMap).
        let s = serde_json::to_string(&v.to_json()).unwrap();
        assert_eq!(s, r#"{"b":1,"a":[true,"x"]}"#);
    }
}
