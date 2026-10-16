//! GitHub Actions expressions: `${{ … }}` templates and `if:` conditions.
//!
//! Values are JSON values. Semantics follow GitHub: `&&`/`||` return an
//! operand, `==` coerces mismatched scalar types to numbers and compares
//! strings case-insensitively, missing properties are `null`, and a
//! condition without a status function means `success() && (…)`.

use std::cmp::Ordering;

use serde_json::{Map, Value};

/// The job's status as seen by `success()`, `failure()` and `cancelled()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    Success,
    Failure,
    Cancelled,
}

impl JobStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            JobStatus::Success => "success",
            JobStatus::Failure => "failure",
            JobStatus::Cancelled => "cancelled",
        }
    }
}

/// Everything an expression can see: named contexts (`github`, `env`,
/// `steps`, …) and the job status.
#[derive(Debug, Clone)]
pub struct Contexts {
    pub values: Map<String, Value>,
    pub status: JobStatus,
}

impl Contexts {
    pub fn new() -> Self {
        Contexts { values: Map::new(), status: JobStatus::Success }
    }

    pub fn set(&mut self, name: &str, v: Value) {
        self.values.insert(name.to_string(), v);
    }

    /// A context object, created empty if missing.
    pub fn object(&mut self, name: &str) -> &mut Map<String, Value> {
        let v = self.values.entry(name.to_string()).or_insert_with(|| Value::Object(Map::new()));
        if !v.is_object() {
            *v = Value::Object(Map::new());
        }
        v.as_object_mut().expect("object")
    }
}

impl Default for Contexts {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    LParen,
    RParen,
    LBracket,
    RBracket,
    Dot,
    Comma,
    Star,
    Not,
    And,
    Or,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Str(String),
    Num(f64),
    Ident(String),
}

fn lex(s: &str) -> Result<Vec<Tok>, String> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let two = |n: u8| b.get(i + 1) == Some(&n);
        let (tok, len) = match c {
            b' ' | b'\t' | b'\r' | b'\n' => {
                i += 1;
                continue;
            }
            b'(' => (Tok::LParen, 1),
            b')' => (Tok::RParen, 1),
            b'[' => (Tok::LBracket, 1),
            b']' => (Tok::RBracket, 1),
            b'.' => (Tok::Dot, 1),
            b',' => (Tok::Comma, 1),
            b'*' => (Tok::Star, 1),
            b'!' if two(b'=') => (Tok::Ne, 2),
            b'!' => (Tok::Not, 1),
            b'=' if two(b'=') => (Tok::Eq, 2),
            b'<' if two(b'=') => (Tok::Le, 2),
            b'<' => (Tok::Lt, 1),
            b'>' if two(b'=') => (Tok::Ge, 2),
            b'>' => (Tok::Gt, 1),
            b'&' if two(b'&') => (Tok::And, 2),
            b'|' if two(b'|') => (Tok::Or, 2),
            b'\'' => {
                let mut v = String::new();
                let mut j = i + 1;
                loop {
                    match s[j..].find('\'') {
                        None => return Err(format!("unterminated string in {s:?}")),
                        Some(k) => {
                            v.push_str(&s[j..j + k]);
                            j += k + 1;
                            if b.get(j) == Some(&b'\'') {
                                v.push('\'');
                                j += 1;
                            } else {
                                break;
                            }
                        }
                    }
                }
                out.push(Tok::Str(v));
                i = j;
                continue;
            }
            c if c.is_ascii_digit() || (c == b'-' && b.get(i + 1).is_some_and(|d| d.is_ascii_digit() || *d == b'.')) => {
                let mut j = i + 1;
                while j < b.len() {
                    let d = b[j];
                    let exp_sign = (d == b'+' || d == b'-') && matches!(b[j - 1], b'e' | b'E') && !s[i..j].contains(['x', 'X']);
                    if d.is_ascii_alphanumeric() || d == b'.' || exp_sign {
                        j += 1;
                    } else {
                        break;
                    }
                }
                let text = &s[i..j];
                let n = parse_number(text).ok_or_else(|| format!("invalid number {text:?}"))?;
                out.push(Tok::Num(n));
                i = j;
                continue;
            }
            c if c.is_ascii_alphabetic() || c == b'_' => {
                let mut j = i + 1;
                while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_' || b[j] == b'-') {
                    j += 1;
                }
                out.push(Tok::Ident(s[i..j].to_string()));
                i = j;
                continue;
            }
            _ => return Err(format!("unexpected character {:?} in {s:?}", &s[i..].chars().next().unwrap_or(' '))),
        };
        out.push(tok);
        i += len;
    }
    Ok(out)
}

fn parse_number(text: &str) -> Option<f64> {
    let t = text.trim();
    let (neg, body) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t),
    };
    let n = if let Some(h) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        i64::from_str_radix(h, 16).ok()? as f64
    } else if let Some(o) = body.strip_prefix("0o") {
        i64::from_str_radix(o, 8).ok()? as f64
    } else {
        if body.is_empty() || !body.bytes().all(|c| c.is_ascii_digit() || matches!(c, b'.' | b'e' | b'E' | b'+' | b'-')) {
            return None;
        }
        body.parse::<f64>().ok()?
    };
    Some(if neg { -n } else { n })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// A parsed expression.
#[derive(Debug, Clone)]
pub enum Expr {
    Lit(Value),
    Ctx(String),
    Prop(Box<Expr>, String),
    Index(Box<Expr>, Box<Expr>),
    Filter(Box<Expr>),
    Not(Box<Expr>),
    Cmp(Cmp, Box<Expr>, Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Call(String, Vec<Expr>),
}

const STATUS_FUNCTIONS: [&str; 4] = ["success", "failure", "always", "cancelled"];

impl Expr {
    /// Does the expression call `success()`, `failure()`, `always()` or
    /// `cancelled()`?
    pub fn has_status_function(&self) -> bool {
        match self {
            Expr::Lit(_) | Expr::Ctx(_) => false,
            Expr::Call(name, args) => STATUS_FUNCTIONS.contains(&name.as_str()) || args.iter().any(Expr::has_status_function),
            Expr::Prop(e, _) | Expr::Filter(e) | Expr::Not(e) => e.has_status_function(),
            Expr::Index(a, b) | Expr::Cmp(_, a, b) | Expr::And(a, b) | Expr::Or(a, b) => {
                a.has_status_function() || b.has_status_function()
            }
        }
    }
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

impl Parser {
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

    fn expect(&mut self, t: &Tok) -> Result<(), String> {
        if self.eat(t) { Ok(()) } else { Err(format!("expected {t:?}, found {:?}", self.peek())) }
    }

    fn or(&mut self) -> Result<Expr, String> {
        let mut l = self.and()?;
        while self.eat(&Tok::Or) {
            l = Expr::Or(Box::new(l), Box::new(self.and()?));
        }
        Ok(l)
    }

    fn and(&mut self) -> Result<Expr, String> {
        let mut l = self.equality()?;
        while self.eat(&Tok::And) {
            l = Expr::And(Box::new(l), Box::new(self.equality()?));
        }
        Ok(l)
    }

    fn equality(&mut self) -> Result<Expr, String> {
        let mut l = self.comparison()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Eq) => Cmp::Eq,
                Some(Tok::Ne) => Cmp::Ne,
                _ => return Ok(l),
            };
            self.pos += 1;
            l = Expr::Cmp(op, Box::new(l), Box::new(self.comparison()?));
        }
    }

    fn comparison(&mut self) -> Result<Expr, String> {
        let mut l = self.unary()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Lt) => Cmp::Lt,
                Some(Tok::Le) => Cmp::Le,
                Some(Tok::Gt) => Cmp::Gt,
                Some(Tok::Ge) => Cmp::Ge,
                _ => return Ok(l),
            };
            self.pos += 1;
            l = Expr::Cmp(op, Box::new(l), Box::new(self.unary()?));
        }
    }

    fn unary(&mut self) -> Result<Expr, String> {
        if self.eat(&Tok::Not) {
            return Ok(Expr::Not(Box::new(self.unary()?)));
        }
        self.postfix()
    }

    fn postfix(&mut self) -> Result<Expr, String> {
        let mut e = self.primary()?;
        loop {
            if self.eat(&Tok::Dot) {
                e = match self.next() {
                    Some(Tok::Ident(name)) => Expr::Prop(Box::new(e), name),
                    Some(Tok::Star) => Expr::Filter(Box::new(e)),
                    // `steps.1.outputs` style names.
                    Some(Tok::Num(n)) => Expr::Prop(Box::new(e), fmt_num(n)),
                    t => return Err(format!("expected a property name after '.', found {t:?}")),
                };
            } else if self.eat(&Tok::LBracket) {
                if self.eat(&Tok::Star) {
                    e = Expr::Filter(Box::new(e));
                } else {
                    e = Expr::Index(Box::new(e), Box::new(self.or()?));
                }
                self.expect(&Tok::RBracket)?;
            } else {
                return Ok(e);
            }
        }
    }

    fn primary(&mut self) -> Result<Expr, String> {
        match self.next() {
            Some(Tok::Str(s)) => Ok(Expr::Lit(Value::String(s))),
            Some(Tok::Num(n)) => Ok(Expr::Lit(num_value(n))),
            Some(Tok::LParen) => {
                let e = self.or()?;
                self.expect(&Tok::RParen)?;
                Ok(e)
            }
            Some(Tok::Ident(name)) => {
                if self.eat(&Tok::LParen) {
                    let mut args = Vec::new();
                    if !self.eat(&Tok::RParen) {
                        loop {
                            args.push(self.or()?);
                            if self.eat(&Tok::RParen) {
                                break;
                            }
                            self.expect(&Tok::Comma)?;
                        }
                    }
                    return Ok(Expr::Call(name.to_ascii_lowercase(), args));
                }
                Ok(match name.as_str() {
                    "true" => Expr::Lit(Value::Bool(true)),
                    "false" => Expr::Lit(Value::Bool(false)),
                    "null" => Expr::Lit(Value::Null),
                    _ => Expr::Ctx(name),
                })
            }
            t => Err(format!("unexpected {t:?}")),
        }
    }
}

/// Parse an expression (without the `${{ }}` delimiters).
pub fn parse(s: &str) -> Result<Expr, String> {
    let toks = lex(s)?;
    if toks.is_empty() {
        return Err("empty expression".into());
    }
    let mut p = Parser { toks, pos: 0 };
    let e = p.or()?;
    if p.pos < p.toks.len() {
        return Err(format!("unexpected {:?} in {s:?}", p.toks[p.pos]));
    }
    Ok(e)
}

fn num_value(n: f64) -> Value {
    if n.fract() == 0.0 && n.abs() < 9e15 {
        Value::from(n as i64)
    } else {
        serde_json::Number::from_f64(n).map(Value::Number).unwrap_or(Value::Null)
    }
}

fn fmt_num(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 9e15 { format!("{}", n as i64) } else { format!("{n}") }
}

pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// A value as text, as substituted into templates.
pub fn to_text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.as_f64().map(fmt_num).unwrap_or_else(|| n.to_string()),
        Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    }
}

fn to_number(v: &Value) -> f64 {
    match v {
        Value::Null => 0.0,
        Value::Bool(b) => f64::from(u8::from(*b)),
        Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
        Value::String(s) if s.trim().is_empty() => 0.0,
        Value::String(s) => parse_number(s).unwrap_or(f64::NAN),
        _ => f64::NAN,
    }
}

fn loose_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::String(x), Value::String(y)) => x.to_lowercase() == y.to_lowercase(),
        (Value::Array(_) | Value::Object(_), _) | (_, Value::Array(_) | Value::Object(_)) => a == b,
        _ => to_number(a) == to_number(b),
    }
}

fn compare(a: &Value, b: &Value) -> Option<Ordering> {
    match (a, b) {
        (Value::String(x), Value::String(y)) => Some(x.to_lowercase().cmp(&y.to_lowercase())),
        _ => to_number(a).partial_cmp(&to_number(b)),
    }
}

fn lookup<'a>(m: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    m.get(key).or_else(|| m.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v))
}

fn prop(v: Value, key: &str) -> Value {
    match v {
        Value::Object(m) => lookup(&m, key).cloned().unwrap_or(Value::Null),
        // After a `.*` filter: project every element.
        Value::Array(a) => Value::Array(a.into_iter().map(|e| prop(e, key)).filter(|e| !e.is_null()).collect()),
        _ => Value::Null,
    }
}

fn format_fn(fmt: &str, args: &[Value]) -> Result<String, String> {
    let mut out = String::new();
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                out.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                out.push('}');
            }
            '{' => {
                let mut idx = String::new();
                loop {
                    match chars.next() {
                        Some('}') => break,
                        Some(d) if d.is_ascii_digit() => idx.push(d),
                        _ => return Err(format!("format(): invalid format string {fmt:?}")),
                    }
                }
                let i: usize = idx.parse().map_err(|_| format!("format(): invalid format string {fmt:?}"))?;
                let a = args.get(i).ok_or_else(|| format!("format(): no argument {{{i}}} for {fmt:?}"))?;
                out.push_str(&to_text(a));
            }
            '}' => return Err(format!("format(): unmatched '}}' in {fmt:?}")),
            c => out.push(c),
        }
    }
    Ok(out)
}

fn call(name: &str, args: &[Expr], c: &Contexts) -> Result<Value, String> {
    let arity = |min: usize, max: usize| {
        if args.len() < min || args.len() > max {
            Err(format!("{name}() takes {min}{} argument(s), got {}", if max > min { "+" } else { "" }, args.len()))
        } else {
            Ok(())
        }
    };
    let mut vals = Vec::with_capacity(args.len());
    let status_fn = STATUS_FUNCTIONS.contains(&name);
    if !status_fn {
        for a in args {
            vals.push(eval(a, c)?);
        }
    }
    let text = |i: usize| to_text(&vals[i]).to_lowercase();
    Ok(match name {
        "success" => {
            arity(0, 0)?;
            Value::Bool(c.status == JobStatus::Success)
        }
        "failure" => {
            arity(0, 0)?;
            Value::Bool(c.status == JobStatus::Failure)
        }
        "cancelled" => {
            arity(0, 0)?;
            Value::Bool(c.status == JobStatus::Cancelled)
        }
        "always" => {
            arity(0, 0)?;
            Value::Bool(true)
        }
        "contains" => {
            arity(2, 2)?;
            match &vals[0] {
                Value::Array(a) => Value::Bool(a.iter().any(|e| loose_eq(e, &vals[1]))),
                _ => Value::Bool(text(0).contains(&text(1))),
            }
        }
        "startswith" => {
            arity(2, 2)?;
            Value::Bool(text(0).starts_with(&text(1)))
        }
        "endswith" => {
            arity(2, 2)?;
            Value::Bool(text(0).ends_with(&text(1)))
        }
        "format" => {
            arity(1, usize::MAX)?;
            Value::String(format_fn(&to_text(&vals[0]), &vals[1..])?)
        }
        "join" => {
            arity(1, 2)?;
            let sep = vals.get(1).map(to_text).unwrap_or_else(|| ",".into());
            match &vals[0] {
                Value::Array(a) => Value::String(a.iter().map(to_text).collect::<Vec<_>>().join(&sep)),
                v => Value::String(to_text(v)),
            }
        }
        "tojson" => {
            arity(1, 1)?;
            Value::String(serde_json::to_string_pretty(&vals[0]).map_err(|e| e.to_string())?)
        }
        "fromjson" => {
            arity(1, 1)?;
            let s = to_text(&vals[0]);
            serde_json::from_str(&s).map_err(|e| format!("fromJSON(): {e}"))?
        }
        "hashfiles" => return Err("hashFiles() is not supported by flux-runner".into()),
        other => return Err(format!("unknown function {other}()")),
    })
}

/// Evaluate a parsed expression.
pub fn eval(e: &Expr, c: &Contexts) -> Result<Value, String> {
    Ok(match e {
        Expr::Lit(v) => v.clone(),
        Expr::Ctx(name) => lookup(&c.values, name).cloned().ok_or_else(|| format!("unrecognized named-value: '{name}'"))?,
        Expr::Prop(obj, key) => prop(eval(obj, c)?, key),
        Expr::Index(obj, idx) => {
            let (o, i) = (eval(obj, c)?, eval(idx, c)?);
            match (o, &i) {
                (Value::Array(a), Value::Number(n)) => {
                    n.as_f64().filter(|f| *f >= 0.0).and_then(|f| a.get(f as usize).cloned()).unwrap_or(Value::Null)
                }
                (o, _) => prop(o, &to_text(&i)),
            }
        }
        Expr::Filter(obj) => match eval(obj, c)? {
            Value::Object(m) => Value::Array(m.into_iter().map(|(_, v)| v).collect()),
            a @ Value::Array(_) => a,
            _ => Value::Array(vec![]),
        },
        Expr::Not(x) => Value::Bool(!truthy(&eval(x, c)?)),
        Expr::And(a, b) => {
            let l = eval(a, c)?;
            if !truthy(&l) {
                return Ok(l);
            }
            eval(b, c)?
        }
        Expr::Or(a, b) => {
            let l = eval(a, c)?;
            if truthy(&l) {
                return Ok(l);
            }
            eval(b, c)?
        }
        Expr::Cmp(op, a, b) => {
            let (l, r) = (eval(a, c)?, eval(b, c)?);
            Value::Bool(match op {
                Cmp::Eq => loose_eq(&l, &r),
                Cmp::Ne => !loose_eq(&l, &r),
                Cmp::Lt => compare(&l, &r) == Some(Ordering::Less),
                Cmp::Le => matches!(compare(&l, &r), Some(Ordering::Less | Ordering::Equal)),
                Cmp::Gt => compare(&l, &r) == Some(Ordering::Greater),
                Cmp::Ge => matches!(compare(&l, &r), Some(Ordering::Greater | Ordering::Equal)),
            })
        }
        Expr::Call(name, args) => call(name, args, c)?,
    })
}

/// Parse and evaluate an expression string.
pub fn evaluate(s: &str, c: &Contexts) -> Result<Value, String> {
    eval(&parse(s)?, c)
}

/// Find the end of a `${{ … }}` body starting at `from` (the index after
/// `${{`), skipping `}}` inside string literals. Returns the index of `}}`.
fn template_end(s: &str, from: usize) -> Option<usize> {
    let b = s.as_bytes();
    let mut i = from;
    let mut quoted = false;
    while i < b.len() {
        match b[i] {
            b'\'' => quoted = !quoted,
            b'}' if !quoted && b.get(i + 1) == Some(&b'}') => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

/// Replace every `${{ expr }}` in `s` with its value.
pub fn interpolate(s: &str, c: &Contexts) -> Result<String, String> {
    if !s.contains("${{") {
        return Ok(s.to_string());
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = 0;
    while let Some(off) = s[rest..].find("${{") {
        let start = rest + off;
        out.push_str(&s[rest..start]);
        let end = template_end(s, start + 3).ok_or_else(|| format!("unterminated ${{{{ in {s:?}"))?;
        out.push_str(&to_text(&evaluate(&s[start + 3..end], c)?));
        rest = end + 2;
    }
    out.push_str(&s[rest..]);
    Ok(out)
}

/// Evaluate an `if:` condition. `None` (or empty) means `success()`; a bare
/// expression may omit `${{ }}`; a condition without a status function only
/// passes while the job is succeeding.
pub fn condition(cond: Option<&str>, c: &Contexts) -> Result<bool, String> {
    let s = cond.map(str::trim).unwrap_or_default();
    if s.is_empty() {
        return Ok(c.status == JobStatus::Success);
    }
    let whole = s.strip_prefix("${{").and_then(|inner| {
        let end = template_end(s, 3)?;
        (end + 2 == s.len()).then(|| &inner[..end - 3])
    });
    let expr = match whole {
        Some(inner) => inner.to_string(),
        None if s.contains("${{") => {
            // Text mixed with templates is a string: true unless empty.
            return Ok(c.status == JobStatus::Success && !interpolate(s, c)?.is_empty());
        }
        None => s.to_string(),
    };
    let e = parse(&expr)?;
    if !e.has_status_function() && c.status != JobStatus::Success {
        return Ok(false);
    }
    Ok(truthy(&eval(&e, c)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx() -> Contexts {
        let mut c = Contexts::new();
        c.set("github", json!({ "ref": "refs/heads/main", "event_name": "push", "sha": "abc123", "run_id": 7 }));
        c.set("env", json!({ "NAME": "flux", "EMPTY": "" }));
        c.set("matrix", json!({ "os": "linux", "n": 3, "flag": true }));
        c.set("steps", json!({ "build-x": { "outputs": { "y": "42", "list": "[1,2]" }, "outcome": "success" } }));
        c.set("secrets", json!({ "TOKEN": "s3cr3t" }));
        c.set("runner", json!({ "os": "Linux", "arch": "X64" }));
        c
    }

    fn ev(s: &str) -> Value {
        evaluate(s, &ctx()).unwrap_or_else(|e| panic!("{s}: {e}"))
    }

    #[test]
    fn literals_and_operators() {
        assert_eq!(ev("'it''s'"), json!("it's"));
        assert_eq!(ev("1.5"), json!(1.5));
        assert_eq!(ev("-2"), json!(-2));
        assert_eq!(ev("0xff"), json!(255));
        assert_eq!(ev("null"), Value::Null);
        assert_eq!(ev("!true"), json!(false));
        assert_eq!(ev("!!'x'"), json!(true));
        assert_eq!(ev("1 == 1.0"), json!(true));
        assert_eq!(ev("'ABC' == 'abc'"), json!(true));
        assert_eq!(ev("'1' == 1"), json!(true));
        assert_eq!(ev("true == 1"), json!(true));
        assert_eq!(ev("null == 0"), json!(true));
        assert_eq!(ev("'x' != 'y'"), json!(true));
        assert_eq!(ev("2 > 1 && 1 >= 1 && 0 < 1 && 1 <= 1"), json!(true));
        assert_eq!(ev("'b' > 'A'"), json!(true));
        assert_eq!(ev("'a' < 1"), json!(false));
    }

    #[test]
    fn precedence_and_short_circuit() {
        assert_eq!(ev("true || false && false"), json!(true));
        assert_eq!(ev("(true || false) && false"), json!(false));
        assert_eq!(ev("!false && false"), json!(false));
        assert_eq!(ev("!(false && false)"), json!(true));
        assert_eq!(ev("1 == 1 && 2 == 3 || 'x' == 'x'"), json!(true));
        assert_eq!(ev("1 < 2 == true"), json!(true));
        // `&&` / `||` return operands.
        assert_eq!(ev("env.EMPTY || 'default'"), json!("default"));
        assert_eq!(ev("env.NAME && 'yes'"), json!("yes"));
        assert_eq!(ev("matrix.missing && nope.x"), Value::Null);
        assert!(evaluate("nope.x", &ctx()).unwrap_err().contains("nope"));
    }

    #[test]
    fn contexts() {
        assert_eq!(ev("github.ref"), json!("refs/heads/main"));
        assert_eq!(ev("GitHub.Event_Name"), json!("push"));
        assert_eq!(ev("github['sha']"), json!("abc123"));
        assert_eq!(ev("steps.build-x.outputs.y"), json!("42"));
        assert_eq!(ev("steps['build-x'].outcome"), json!("success"));
        assert_eq!(ev("steps.missing.outputs.y"), Value::Null);
        assert_eq!(ev("matrix.n == 3 && matrix.flag"), json!(true));
        assert_eq!(ev("secrets.TOKEN"), json!("s3cr3t"));
        assert_eq!(ev("runner.os == 'linux'"), json!(true));
        assert_eq!(ev("fromJSON(steps.build-x.outputs.list)[1]"), json!(2));
        assert_eq!(ev("steps.*.outcome"), json!(["success"]));
    }

    #[test]
    fn functions() {
        assert_eq!(ev("contains('Hello world', 'WORLD')"), json!(true));
        assert_eq!(ev("contains(fromJSON('[\"a\", \"b\"]'), 'B')"), json!(true));
        assert_eq!(ev("contains(fromJSON('[1, 2]'), 3)"), json!(false));
        assert_eq!(ev("startsWith(github.ref, 'refs/heads/')"), json!(true));
        assert_eq!(ev("endsWith(github.ref, '/MAIN')"), json!(true));
        assert_eq!(ev("format('{0}-{1}-{0} {{x}}', 'a', 2)"), json!("a-2-a {x}"));
        assert!(evaluate("format('{1}', 'a')", &ctx()).is_err());
        assert_eq!(ev("join(fromJSON('[\"a\", 1, true]'), ', ')"), json!("a, 1, true"));
        assert_eq!(ev("join(fromJSON('[\"a\", \"b\"]'))"), json!("a,b"));
        assert_eq!(ev("toJSON(matrix.n)"), json!("3"));
        assert_eq!(ev("fromJSON(toJSON(matrix)).os"), json!("linux"));
        assert_eq!(ev("fromJSON('{\"a\": {\"b\": null}}').a.b"), Value::Null);
        assert_eq!(ev("success() && !failure() && always() && !cancelled()"), json!(true));
        assert!(evaluate("nosuch()", &ctx()).is_err());
        assert!(evaluate("contains('a')", &ctx()).is_err());
        assert!(evaluate("hashFiles('**/Cargo.lock')", &ctx()).is_err());
    }

    #[test]
    fn templates() {
        let c = ctx();
        assert_eq!(interpolate("no templates", &c).unwrap(), "no templates");
        assert_eq!(interpolate("${{ matrix.os }}-${{matrix.n}}", &c).unwrap(), "linux-3");
        assert_eq!(interpolate("x ${{ 'a}}b' }} y", &c).unwrap(), "x a}}b y");
        assert_eq!(interpolate("${{ matrix.missing }}|", &c).unwrap(), "|");
        assert_eq!(interpolate("${{ 1.50 }} ${{ true }}", &c).unwrap(), "1.5 true");
        assert!(interpolate("${{ matrix.os ", &c).is_err());
    }

    #[test]
    fn conditions() {
        let mut c = ctx();
        assert!(condition(None, &c).unwrap());
        assert!(condition(Some("github.event_name == 'push'"), &c).unwrap());
        assert!(condition(Some("${{ github.event_name == 'push' }}"), &c).unwrap());
        assert!(!condition(Some("${{ github.event_name == 'pull_request' }}"), &c).unwrap());
        assert!(condition(Some("${{ false }} || true"), &c).unwrap(), "mixed text is a non-empty string");
        assert!(!condition(Some("failure()"), &c).unwrap());
        c.status = JobStatus::Failure;
        assert!(!condition(None, &c).unwrap());
        assert!(!condition(Some("true"), &c).unwrap(), "implicit success()");
        assert!(condition(Some("failure()"), &c).unwrap());
        assert!(condition(Some("always()"), &c).unwrap());
        assert!(condition(Some("${{ !cancelled() }}"), &c).unwrap());
        assert!(!condition(Some("success() || cancelled()"), &c).unwrap());
        c.status = JobStatus::Cancelled;
        assert!(condition(Some("cancelled()"), &c).unwrap());
        assert!(!condition(Some("failure()"), &c).unwrap());
        assert!(condition(Some("a b"), &ctx()).is_err());
        assert!(condition(Some("(true"), &ctx()).is_err());
    }
}
