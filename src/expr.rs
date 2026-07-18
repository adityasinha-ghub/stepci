//! Evaluate GitHub Actions `${{ }}` expressions.
//!
//! Pipeline: [`lex`] → [`Parser`] (recursive descent by precedence) → [`eval`].
//! Two entry points are exposed: [`evaluate`] for a bare expression and
//! [`interpolate`] for a template string that mixes literal text with `${{ }}`
//! spans (used for `run:` bodies and `env`/`with` values).
//!
//! Semantics follow GitHub: `&&`/`||` return an *operand* (not a bool), equality
//! is loose (case-insensitive for strings; reference-based for aggregates),
//! relational operators compare strings/booleans in-kind and coerce mixed types
//! to numbers, and accessing a missing property yields `null` rather than erroring.

use anyhow::{Result, bail};
use indexmap::IndexMap;

use crate::value::Value;

/// The job status, so `success()`/`failure()`/`cancelled()` can be evaluated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum JobStatus {
    #[default]
    Success,
    Failure,
    Cancelled,
}

/// Everything an expression can read: the named contexts (`github`, `env`,
/// `steps`, `matrix`, …) plus the current job status.
#[derive(Debug, Clone, Default)]
pub struct Context {
    /// Top-level context objects, keyed by name.
    pub vars: IndexMap<String, Value>,
    /// Current status, for the status functions.
    pub status: JobStatus,
}

impl Context {
    /// Convenience constructor for a context with no status override.
    pub fn new(vars: IndexMap<String, Value>) -> Self {
        Context {
            vars,
            status: JobStatus::Success,
        }
    }
}

/// Evaluate a single expression (the text inside `${{ }}`) to a [`Value`].
pub fn evaluate(src: &str, ctx: &Context) -> Result<Value> {
    let tokens = lex(src)?;
    let mut parser = Parser::new(tokens);
    let ast = parser.parse_expr()?;
    parser.expect_eof()?;
    eval(&ast, ctx)
}

/// Interpolate a template: literal text is kept verbatim, and each `${{ … }}`
/// span is evaluated and replaced with its string coercion.
pub fn interpolate(template: &str, ctx: &Context) -> Result<String> {
    let mut out = String::new();
    let bytes = template.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && template[i..].starts_with("${{") {
            let rest = &template[i + 3..];
            let end = find_close(rest)
                .ok_or_else(|| anyhow::anyhow!("unterminated `${{{{` expression"))?;
            let expr = &rest[..end];
            let value = evaluate(expr, ctx)?;
            out.push_str(&value.to_display_string());
            i += 3 + end + 2; // skip `${{ … }}`
        } else {
            let ch = template[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    Ok(out)
}

/// Find the byte offset of the `}}` that closes a `${{`, skipping any `}}` that
/// appears inside a single-quoted string literal.
fn find_close(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let mut i = 0;
    let mut in_str = false;
    while i < b.len() {
        if in_str {
            if b[i] == b'\'' {
                // `''` is an escaped quote inside the string.
                if s[i + 1..].starts_with('\'') {
                    i += 2;
                    continue;
                }
                in_str = false;
            }
            i += 1;
        } else if b[i] == b'\'' {
            in_str = true;
            i += 1;
        } else if b[i] == b'}' && s[i..].starts_with("}}") {
            return Some(i);
        } else {
            i += 1;
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Lexer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Number(f64),
    Str(String),
    Ident(String),
    True,
    False,
    Null,
    Not,
    And,
    Or,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Dot,
    Comma,
    Star,
    LParen,
    RParen,
    LBracket,
    RBracket,
}

fn lex(src: &str) -> Result<Vec<Tok>> {
    let chars: Vec<char> = src.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            c if c.is_whitespace() => i += 1,
            '(' => push(&mut toks, Tok::LParen, &mut i),
            ')' => push(&mut toks, Tok::RParen, &mut i),
            '[' => push(&mut toks, Tok::LBracket, &mut i),
            ']' => push(&mut toks, Tok::RBracket, &mut i),
            '.' if !next_is_digit(&chars, i) => push(&mut toks, Tok::Dot, &mut i),
            ',' => push(&mut toks, Tok::Comma, &mut i),
            '*' => push(&mut toks, Tok::Star, &mut i),
            '!' if peek(&chars, i + 1) == Some('=') => push2(&mut toks, Tok::Ne, &mut i),
            '!' => push(&mut toks, Tok::Not, &mut i),
            '=' if peek(&chars, i + 1) == Some('=') => push2(&mut toks, Tok::Eq, &mut i),
            '<' if peek(&chars, i + 1) == Some('=') => push2(&mut toks, Tok::Le, &mut i),
            '<' => push(&mut toks, Tok::Lt, &mut i),
            '>' if peek(&chars, i + 1) == Some('=') => push2(&mut toks, Tok::Ge, &mut i),
            '>' => push(&mut toks, Tok::Gt, &mut i),
            '&' if peek(&chars, i + 1) == Some('&') => push2(&mut toks, Tok::And, &mut i),
            '|' if peek(&chars, i + 1) == Some('|') => push2(&mut toks, Tok::Or, &mut i),
            '\'' => toks.push(lex_string(&chars, &mut i)?),
            c if c.is_ascii_digit() || (c == '-' && next_is_digit(&chars, i)) || c == '.' => {
                toks.push(lex_number(&chars, &mut i)?)
            }
            c if is_ident_start(c) => toks.push(lex_ident(&chars, &mut i)),
            _ => bail!("unexpected character `{c}` in expression"),
        }
    }
    Ok(toks)
}

fn push(toks: &mut Vec<Tok>, t: Tok, i: &mut usize) {
    toks.push(t);
    *i += 1;
}

fn push2(toks: &mut Vec<Tok>, t: Tok, i: &mut usize) {
    toks.push(t);
    *i += 2;
}

fn peek(chars: &[char], i: usize) -> Option<char> {
    chars.get(i).copied()
}

fn next_is_digit(chars: &[char], i: usize) -> bool {
    chars.get(i + 1).is_some_and(|c| c.is_ascii_digit())
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn is_ident_part(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

fn lex_string(chars: &[char], i: &mut usize) -> Result<Tok> {
    *i += 1; // opening quote
    let mut s = String::new();
    loop {
        match chars.get(*i) {
            None => bail!("unterminated string literal in expression"),
            Some('\'') => {
                // `''` → a literal single quote.
                if chars.get(*i + 1) == Some(&'\'') {
                    s.push('\'');
                    *i += 2;
                } else {
                    *i += 1;
                    return Ok(Tok::Str(s));
                }
            }
            Some(&c) => {
                s.push(c);
                *i += 1;
            }
        }
    }
}

fn lex_number(chars: &[char], i: &mut usize) -> Result<Tok> {
    let start = *i;
    if chars.get(*i) == Some(&'-') {
        *i += 1;
    }
    // Hex literal.
    if chars.get(*i) == Some(&'0') && matches!(chars.get(*i + 1), Some('x') | Some('X')) {
        *i += 2;
        while chars.get(*i).is_some_and(|c| c.is_ascii_hexdigit()) {
            *i += 1;
        }
    } else {
        while chars
            .get(*i)
            .is_some_and(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-'))
        {
            *i += 1;
        }
    }
    let text: String = chars[start..*i].iter().collect();
    let n = parse_number_literal(&text)
        .ok_or_else(|| anyhow::anyhow!("invalid number literal `{text}` in expression"))?;
    Ok(Tok::Number(n))
}

fn parse_number_literal(text: &str) -> Option<f64> {
    if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        return i64::from_str_radix(hex, 16).ok().map(|i| i as f64);
    }
    text.parse::<f64>().ok()
}

fn lex_ident(chars: &[char], i: &mut usize) -> Tok {
    let start = *i;
    while chars.get(*i).is_some_and(|&c| is_ident_part(c)) {
        *i += 1;
    }
    let word: String = chars[start..*i].iter().collect();
    match word.to_ascii_lowercase().as_str() {
        "true" => Tok::True,
        "false" => Tok::False,
        "null" => Tok::Null,
        _ => Tok::Ident(word),
    }
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum BinOp {
    And,
    Or,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Debug, Clone, PartialEq)]
enum Ast {
    Null,
    Bool(bool),
    Number(f64),
    Str(String),
    Ident(String),
    Property(Box<Ast>, String),
    Index(Box<Ast>, Box<Ast>),
    Star(Box<Ast>),
    Call(String, Vec<Ast>),
    Not(Box<Ast>),
    Binary(BinOp, Box<Ast>, Box<Ast>),
}

/// Cap on expression nesting depth, so pathological input (deeply nested parens
/// or `!` chains) errors instead of overflowing the stack — an uncatchable abort.
const MAX_DEPTH: usize = 128;

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
    depth: usize,
}

impl Parser {
    fn new(toks: Vec<Tok>) -> Self {
        Parser {
            toks,
            pos: 0,
            depth: 0,
        }
    }

    /// Enter a recursive parse level, failing if we've nested too deep.
    fn descend(&mut self) -> Result<()> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            bail!("expression nested too deeply (limit {MAX_DEPTH})");
        }
        Ok(())
    }

    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).cloned();
        self.pos += 1;
        t
    }

    fn eat(&mut self, t: &Tok) -> bool {
        if self.peek() == Some(t) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect_eof(&self) -> Result<()> {
        match self.peek() {
            None => Ok(()),
            Some(t) => bail!("unexpected trailing tokens in expression (near {t:?})"),
        }
    }

    fn parse_expr(&mut self) -> Result<Ast> {
        self.descend()?;
        let node = self.parse_or();
        self.depth -= 1;
        node
    }

    fn parse_or(&mut self) -> Result<Ast> {
        let mut left = self.parse_and()?;
        while self.eat(&Tok::Or) {
            let right = self.parse_and()?;
            left = Ast::Binary(BinOp::Or, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Ast> {
        let mut left = self.parse_equality()?;
        while self.eat(&Tok::And) {
            let right = self.parse_equality()?;
            left = Ast::Binary(BinOp::And, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_equality(&mut self) -> Result<Ast> {
        let mut left = self.parse_comparison()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Eq) => BinOp::Eq,
                Some(Tok::Ne) => BinOp::Ne,
                _ => break,
            };
            self.pos += 1;
            let right = self.parse_comparison()?;
            left = Ast::Binary(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_comparison(&mut self) -> Result<Ast> {
        let mut left = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Lt) => BinOp::Lt,
                Some(Tok::Le) => BinOp::Le,
                Some(Tok::Gt) => BinOp::Gt,
                Some(Tok::Ge) => BinOp::Ge,
                _ => break,
            };
            self.pos += 1;
            let right = self.parse_unary()?;
            left = Ast::Binary(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<Ast> {
        if self.eat(&Tok::Not) {
            self.descend()?;
            let inner = self.parse_unary()?;
            self.depth -= 1;
            return Ok(Ast::Not(Box::new(inner)));
        }
        self.parse_postfix()
    }

    fn parse_postfix(&mut self) -> Result<Ast> {
        let mut node = self.parse_primary()?;
        loop {
            if self.eat(&Tok::Dot) {
                match self.next() {
                    Some(Tok::Ident(name)) => node = Ast::Property(Box::new(node), name),
                    Some(Tok::Star) => node = Ast::Star(Box::new(node)),
                    other => bail!("expected a property name after `.`, found {other:?}"),
                }
            } else if self.eat(&Tok::LBracket) {
                let index = self.parse_expr()?;
                if !self.eat(&Tok::RBracket) {
                    bail!("expected `]` to close an index expression");
                }
                node = Ast::Index(Box::new(node), Box::new(index));
            } else {
                break;
            }
        }
        Ok(node)
    }

    fn parse_primary(&mut self) -> Result<Ast> {
        match self.next() {
            Some(Tok::Number(n)) => Ok(Ast::Number(n)),
            Some(Tok::Str(s)) => Ok(Ast::Str(s)),
            Some(Tok::True) => Ok(Ast::Bool(true)),
            Some(Tok::False) => Ok(Ast::Bool(false)),
            Some(Tok::Null) => Ok(Ast::Null),
            Some(Tok::Star) => Ok(Ast::Ident("*".to_string())), // bare `*`, rare
            Some(Tok::LParen) => {
                let inner = self.parse_expr()?;
                if !self.eat(&Tok::RParen) {
                    bail!("expected `)` to close a grouped expression");
                }
                Ok(inner)
            }
            Some(Tok::Ident(name)) => {
                if self.eat(&Tok::LParen) {
                    let args = self.parse_args()?;
                    Ok(Ast::Call(name, args))
                } else {
                    Ok(Ast::Ident(name))
                }
            }
            other => bail!("expected a value in expression, found {other:?}"),
        }
    }

    fn parse_args(&mut self) -> Result<Vec<Ast>> {
        let mut args = Vec::new();
        if self.eat(&Tok::RParen) {
            return Ok(args);
        }
        loop {
            args.push(self.parse_expr()?);
            if self.eat(&Tok::RParen) {
                return Ok(args);
            }
            if !self.eat(&Tok::Comma) {
                bail!("expected `,` or `)` in function arguments");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Evaluator
// ---------------------------------------------------------------------------

fn eval(ast: &Ast, ctx: &Context) -> Result<Value> {
    match ast {
        Ast::Null => Ok(Value::Null),
        Ast::Bool(b) => Ok(Value::Bool(*b)),
        Ast::Number(n) => Ok(Value::Number(*n)),
        Ast::Str(s) => Ok(Value::Str(s.clone())),
        Ast::Ident(name) => Ok(ctx.vars.get(name).cloned().unwrap_or(Value::Null)),
        Ast::Property(obj, name) => Ok(access(&eval(obj, ctx)?, &Value::Str(name.clone()))),
        Ast::Index(obj, idx) => Ok(access(&eval(obj, ctx)?, &eval(idx, ctx)?)),
        Ast::Star(_) => bail!("object filter expressions (`.*`) are not supported yet"),
        Ast::Not(inner) => Ok(Value::Bool(!eval(inner, ctx)?.is_truthy())),
        Ast::Call(name, args) => call(name, args, ctx),
        Ast::Binary(op, l, r) => eval_binary(op, l, r, ctx),
    }
}

/// Property/index access. Missing keys and out-of-range indices yield `null`,
/// matching GitHub (so `github.missing` is `null`, not an error).
fn access(base: &Value, key: &Value) -> Value {
    match base {
        Value::Object(map) => map
            .get(&key.to_display_string())
            .cloned()
            .unwrap_or(Value::Null),
        Value::Array(items) => {
            let n = key.to_number();
            if n >= 0.0 && n == n.trunc() {
                items.get(n as usize).cloned().unwrap_or(Value::Null)
            } else {
                Value::Null
            }
        }
        _ => Value::Null,
    }
}

fn eval_binary(op: &BinOp, l: &Ast, r: &Ast, ctx: &Context) -> Result<Value> {
    match op {
        // `&&`/`||` short-circuit and return an *operand*, not a bool.
        BinOp::And => {
            let left = eval(l, ctx)?;
            if left.is_truthy() {
                eval(r, ctx)
            } else {
                Ok(left)
            }
        }
        BinOp::Or => {
            let left = eval(l, ctx)?;
            if left.is_truthy() {
                Ok(left)
            } else {
                eval(r, ctx)
            }
        }
        BinOp::Eq => Ok(Value::Bool(loose_eq(&eval(l, ctx)?, &eval(r, ctx)?))),
        BinOp::Ne => Ok(Value::Bool(!loose_eq(&eval(l, ctx)?, &eval(r, ctx)?))),
        BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
            Ok(Value::Bool(compare(op, &eval(l, ctx)?, &eval(r, ctx)?)))
        }
    }
}

/// GitHub's loose equality (per the runner's `AbstractEqual`): same-typed scalars
/// compare directly (strings case-insensitively); mixed types coerce to numbers;
/// NaN never equals anything. Arrays/objects use *reference* equality on GitHub —
/// two independently-built aggregates are never equal — so we return `false`.
fn loose_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Number(x), Value::Number(y)) => x == y,
        (Value::Str(x), Value::Str(y)) => x.eq_ignore_ascii_case(y),
        (Value::Array(_), Value::Array(_)) | (Value::Object(_), Value::Object(_)) => false,
        _ => {
            let (x, y) = (a.to_number(), b.to_number());
            x == y && !x.is_nan()
        }
    }
}

/// Relational comparison (`<`, `<=`, `>`, `>=`), matching the runner's
/// `AbstractGreaterThan`/`LessThan`: two strings compare ordinally
/// (case-insensitive), two booleans by boolean order, and every other mix coerces
/// to numbers. Any comparison involving NaN is `false`.
fn compare(op: &BinOp, a: &Value, b: &Value) -> bool {
    use std::cmp::Ordering;
    let ord: Option<Ordering> = match (a, b) {
        (Value::Str(x), Value::Str(y)) => Some(x.to_ascii_lowercase().cmp(&y.to_ascii_lowercase())),
        (Value::Bool(x), Value::Bool(y)) => Some(x.cmp(y)),
        _ => a.to_number().partial_cmp(&b.to_number()), // None when either is NaN
    };
    match ord {
        None => false,
        Some(o) => match op {
            BinOp::Lt => o == Ordering::Less,
            BinOp::Le => o != Ordering::Greater,
            BinOp::Gt => o == Ordering::Greater,
            BinOp::Ge => o != Ordering::Less,
            _ => unreachable!("compare only handles relational operators"),
        },
    }
}

// ---------------------------------------------------------------------------
// Built-in functions
// ---------------------------------------------------------------------------

fn call(name: &str, args: &[Ast], ctx: &Context) -> Result<Value> {
    let vals: Vec<Value> = args.iter().map(|a| eval(a, ctx)).collect::<Result<_>>()?;
    let lname = name.to_ascii_lowercase();
    match lname.as_str() {
        "contains" => {
            check_args(name, &vals, 2)?;
            Ok(Value::Bool(contains(&vals[0], &vals[1])))
        }
        "startswith" => {
            check_args(name, &vals, 2)?;
            Ok(Value::Bool(
                vals[0]
                    .to_display_string()
                    .to_ascii_lowercase()
                    .starts_with(&vals[1].to_display_string().to_ascii_lowercase()),
            ))
        }
        "endswith" => {
            check_args(name, &vals, 2)?;
            Ok(Value::Bool(
                vals[0]
                    .to_display_string()
                    .to_ascii_lowercase()
                    .ends_with(&vals[1].to_display_string().to_ascii_lowercase()),
            ))
        }
        "format" => format_fn(&vals),
        "join" => {
            if vals.is_empty() || vals.len() > 2 {
                bail!("join expects 1 or 2 arguments, got {}", vals.len());
            }
            let sep = vals
                .get(1)
                .map(Value::to_display_string)
                .unwrap_or_else(|| ",".to_string());
            let joined = match &vals[0] {
                Value::Array(items) => items
                    .iter()
                    .map(Value::to_display_string)
                    .collect::<Vec<_>>()
                    .join(&sep),
                other => other.to_display_string(),
            };
            Ok(Value::Str(joined))
        }
        "tojson" => {
            check_args(name, &vals, 1)?;
            Ok(Value::Str(
                serde_json::to_string_pretty(&vals[0].to_json()).unwrap_or_default(),
            ))
        }
        "fromjson" => {
            check_args(name, &vals, 1)?;
            let text = vals[0].to_display_string();
            let parsed: serde_json::Value = serde_json::from_str(&text)
                .map_err(|e| anyhow::anyhow!("fromJSON: invalid JSON: {e}"))?;
            Ok(Value::from_json(parsed))
        }
        "success" => {
            check_args(name, &vals, 0)?;
            Ok(Value::Bool(ctx.status == JobStatus::Success))
        }
        "failure" => {
            check_args(name, &vals, 0)?;
            Ok(Value::Bool(ctx.status == JobStatus::Failure))
        }
        "cancelled" => {
            check_args(name, &vals, 0)?;
            Ok(Value::Bool(ctx.status == JobStatus::Cancelled))
        }
        "always" => {
            check_args(name, &vals, 0)?;
            Ok(Value::Bool(true))
        }
        "hashfiles" => bail!("hashFiles() is not supported yet (arrives with the executor)"),
        _ => bail!("unknown function `{name}`"),
    }
}

fn check_args(name: &str, vals: &[Value], expected: usize) -> Result<()> {
    if vals.len() != expected {
        bail!("{name} expects {expected} argument(s), got {}", vals.len());
    }
    Ok(())
}

fn contains(haystack: &Value, needle: &Value) -> bool {
    match haystack {
        Value::Array(items) => items.iter().any(|item| loose_eq(item, needle)),
        other => other
            .to_display_string()
            .to_ascii_lowercase()
            .contains(&needle.to_display_string().to_ascii_lowercase()),
    }
}

/// `format('{0} {1}', a, b)` with `{{`/`}}` as literal braces.
fn format_fn(vals: &[Value]) -> Result<Value> {
    let Some((fmt, args)) = vals.split_first() else {
        bail!("format expects at least 1 argument");
    };
    let fmt = fmt.to_display_string();
    let chars: Vec<char> = fmt.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '{' if chars.get(i + 1) == Some(&'{') => {
                out.push('{');
                i += 2;
            }
            '}' if chars.get(i + 1) == Some(&'}') => {
                out.push('}');
                i += 2;
            }
            '{' => {
                let mut j = i + 1;
                let mut digits = String::new();
                while chars.get(j).is_some_and(|c| c.is_ascii_digit()) {
                    digits.push(chars[j]);
                    j += 1;
                }
                if digits.is_empty() || chars.get(j) != Some(&'}') {
                    bail!("format: malformed placeholder in `{fmt}`");
                }
                // Parse can overflow on an absurd index like `{999...}`; treat
                // that as "not supplied" rather than panicking.
                let arg = digits
                    .parse::<usize>()
                    .ok()
                    .and_then(|idx| args.get(idx))
                    .ok_or_else(|| {
                        anyhow::anyhow!("format: argument {{{digits}}} was not supplied")
                    })?;
                out.push_str(&arg.to_display_string());
                i = j + 1;
            }
            '}' => bail!("format: unescaped `}}` in `{fmt}` (use `}}}}`)"),
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    Ok(Value::Str(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Value;

    fn ctx() -> Context {
        let mut github = IndexMap::new();
        github.insert("event_name".to_string(), Value::Str("push".to_string()));
        github.insert("ref".to_string(), Value::Str("refs/heads/main".to_string()));
        let mut vars = IndexMap::new();
        vars.insert("github".to_string(), Value::Object(github));
        Context::new(vars)
    }

    fn ev(src: &str) -> Value {
        evaluate(src, &ctx()).unwrap_or_else(|e| panic!("eval `{src}` failed: {e}"))
    }

    #[test]
    fn literals_and_numbers() {
        assert_eq!(ev("true"), Value::Bool(true));
        assert_eq!(ev("null"), Value::Null);
        assert_eq!(ev("42"), Value::Number(42.0));
        assert_eq!(ev("0xff"), Value::Number(255.0));
        assert_eq!(ev("-3"), Value::Number(-3.0));
        assert_eq!(ev("'it''s ok'"), Value::Str("it's ok".to_string()));
    }

    #[test]
    fn context_lookup_and_missing_is_null() {
        assert_eq!(ev("github.event_name"), Value::Str("push".to_string()));
        assert_eq!(ev("github.nope"), Value::Null);
        assert_eq!(ev("totally.unknown"), Value::Null);
        assert_eq!(
            ev("github['ref']"),
            Value::Str("refs/heads/main".to_string())
        );
    }

    #[test]
    fn equality_is_case_insensitive_for_strings() {
        assert_eq!(ev("'PUSH' == 'push'"), Value::Bool(true));
        assert_eq!(ev("github.event_name == 'PUSH'"), Value::Bool(true));
        assert_eq!(ev("'a' != 'b'"), Value::Bool(true));
    }

    #[test]
    fn loose_numeric_comparison() {
        assert_eq!(ev("1 < 2"), Value::Bool(true));
        assert_eq!(ev("'3' == 3"), Value::Bool(true)); // string coerces to number
        assert_eq!(ev("true == 1"), Value::Bool(true));
        assert_eq!(ev("null == 0"), Value::Bool(true));
        assert_eq!(ev("'x' == 0"), Value::Bool(false)); // NaN != 0
    }

    #[test]
    fn and_or_return_operands_and_short_circuit() {
        // `||` yields the first truthy operand (the classic default pattern).
        assert_eq!(ev("'' || 'fallback'"), Value::Str("fallback".to_string()));
        assert_eq!(ev("'set' || 'fallback'"), Value::Str("set".to_string()));
        // `&&` yields the last operand when the first is truthy.
        assert_eq!(ev("true && 'yes'"), Value::Str("yes".to_string()));
        // Short-circuit must NOT evaluate the RHS: `hashFiles` errors if reached,
        // so these only succeed because the RHS is skipped.
        assert_eq!(ev("false && hashFiles('x')"), Value::Bool(false));
        assert_eq!(
            ev("'keep' || hashFiles('x')"),
            Value::Str("keep".to_string())
        );
    }

    #[test]
    fn aggregates_are_never_equal() {
        // GitHub uses reference equality for arrays/objects, so two independently
        // built values are never `==`.
        assert_eq!(
            ev("fromJSON('[1,2]') == fromJSON('[1,2]')"),
            Value::Bool(false)
        );
        assert_eq!(
            ev("fromJSON('{\"a\":1}') == fromJSON('{\"a\":1}')"),
            Value::Bool(false)
        );
    }

    #[test]
    fn relational_strings_and_bools() {
        assert_eq!(ev("'a' < 'b'"), Value::Bool(true)); // ordinal, not numeric
        assert_eq!(ev("'B' > 'a'"), Value::Bool(true)); // case-insensitive
        assert_eq!(ev("'a' <= 'a'"), Value::Bool(true));
        assert_eq!(ev("false < true"), Value::Bool(true)); // bool order
        assert_eq!(ev("2 > 10"), Value::Bool(false)); // still numeric for numbers
    }

    #[test]
    fn format_with_huge_index_errors_not_panics() {
        let err = evaluate("format('{99999999999999999999}', 'a')", &ctx()).unwrap_err();
        assert!(format!("{err:#}").contains("not supplied"), "got: {err:#}");
    }

    #[test]
    fn deeply_nested_expression_errors_instead_of_overflowing() {
        let deep = format!("{}true{}", "!".repeat(5000), "");
        let err = evaluate(&deep, &ctx()).unwrap_err();
        assert!(format!("{err:#}").contains("nested too deeply"));
        let parens = format!("{}1{}", "(".repeat(5000), ")".repeat(5000));
        assert!(evaluate(&parens, &ctx()).is_err());
    }

    #[test]
    fn not_and_precedence() {
        assert_eq!(ev("!false"), Value::Bool(true));
        assert_eq!(ev("!'' == true"), Value::Bool(true)); // (!'' ) == true
        assert_eq!(ev("1 < 2 && 2 < 3"), Value::Bool(true));
    }

    #[test]
    fn functions_contains_startswith_endswith() {
        assert_eq!(ev("contains('hello world', 'World')"), Value::Bool(true));
        assert_eq!(ev("contains(fromJSON('[1,2,3]'), 2)"), Value::Bool(true));
        assert_eq!(
            ev("startsWith('refs/heads/main', 'refs/')"),
            Value::Bool(true)
        );
        assert_eq!(ev("endsWith('main.rs', '.RS')"), Value::Bool(true));
    }

    #[test]
    fn function_format() {
        assert_eq!(
            ev("format('{0}/{1}', 'a', 'b')"),
            Value::Str("a/b".to_string())
        );
        assert_eq!(
            ev("format('{{literal}} {0}', 'x')"),
            Value::Str("{literal} x".to_string())
        );
        let err = evaluate("format('{1}', 'only-zero')", &ctx()).unwrap_err();
        assert!(format!("{err:#}").contains("was not supplied"));
    }

    #[test]
    fn function_join_and_json() {
        assert_eq!(
            ev("join(fromJSON('[\"a\",\"b\"]'), '-')"),
            Value::Str("a-b".to_string())
        );
        assert_eq!(
            ev("join(fromJSON('[1,2,3]'))"),
            Value::Str("1,2,3".to_string())
        );
        assert_eq!(ev("fromJSON('{\"n\": 5}').n"), Value::Number(5.0));
        assert_eq!(ev("toJSON(1)"), Value::Str("1".to_string()));
    }

    #[test]
    fn status_functions() {
        let mut c = ctx();
        c.status = JobStatus::Failure;
        assert_eq!(evaluate("success()", &c).unwrap(), Value::Bool(false));
        assert_eq!(evaluate("failure()", &c).unwrap(), Value::Bool(true));
        assert_eq!(evaluate("always()", &c).unwrap(), Value::Bool(true));
        assert_eq!(evaluate("cancelled()", &c).unwrap(), Value::Bool(false));
    }

    #[test]
    fn interpolation_mixes_literal_and_expressions() {
        let c = ctx();
        assert_eq!(
            interpolate("on ${{ github.event_name }} at ${{ 1 == 1 }}", &c).unwrap(),
            "on push at true"
        );
        // A `}}` inside a string literal must not end the expression early.
        assert_eq!(
            interpolate("${{ contains('a}}b', '}}') }}", &c).unwrap(),
            "true"
        );
        // No expressions → verbatim.
        assert_eq!(interpolate("plain text", &c).unwrap(), "plain text");
    }

    #[test]
    fn errors_are_actionable() {
        assert!(
            format!("{:#}", evaluate("bogus(1)", &ctx()).unwrap_err()).contains("unknown function")
        );
        assert!(
            format!("{:#}", evaluate("hashFiles('*')", &ctx()).unwrap_err())
                .contains("not supported")
        );
        assert!(
            format!("{:#}", evaluate("1 +", &ctx()).unwrap_err()).contains("unexpected character")
        );
        assert!(format!("{:#}", evaluate("(1 == 1", &ctx()).unwrap_err()).contains("expected `)`"));
        assert!(interpolate("${{ 1 == 1", &ctx()).is_err());
    }
}
