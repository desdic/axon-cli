//! Parser for `.http` files (JetBrains / VS Code REST Client format):
//! `###` separators, `# @name`, `@var = value` file variables, and
//! `{{var}}` / `{{$env NAME}}` substitution, and response chaining with
//! `{{NAME.response.body.$.path}}` / `{{NAME.response.headers.Header}}` /
//! `{{NAME.response.cookies.Cookie}}`, filters such as
//! `{{value | split("/", -1) | trim}}`, loops with `# @foreach VAR in {{NAME.response.body.$.path}}`,
//! and response checks with `# @assert`, `# @warn` and `# @notice`.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use serde_json::Value;

pub struct HttpFile {
    /// File variables in definition order, with their values unsubstituted.
    variables: Vec<(String, String)>,
    pub requests: Vec<Request>,
}

/// A request as written in the file, or after [`HttpFile::resolve`].
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    pub name: Option<String>,
    /// Uppercased.
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
    /// Set by `# @foreach VAR in {{...}}`: the request runs once per element.
    pub foreach: Option<Foreach>,
    /// `# @assert`, `# @warn` and `# @notice` checks, in the order written.
    pub checks: Vec<Check>,
    /// 1-based lines of the request's block: from its `###` separator (or the
    /// start of the file) up to the line before the next separator.
    pub start_line: usize,
    pub end_line: usize,
}

/// A `{{NAME.response.body.$.path}}`, `{{NAME.response.headers.Header}}` or
/// `{{NAME.response.cookies.Cookie}}` reference to the response of the request
/// named NAME.
#[derive(Clone, Debug, PartialEq)]
pub struct Reference {
    /// Name of the referenced request; empty for `{{response...}}` in a
    /// check, which reads the checked request's own response.
    pub name: String,
    part: Part,
}

/// How a failed check counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    /// `@assert`: fails the run.
    Assert,
    /// `@warn`: reported; fails the run only with `--strict`.
    Warn,
    /// `@notice`: reported, never fails the run.
    Notice,
}

impl Level {
    const ALL: [Level; 3] = [Level::Assert, Level::Warn, Level::Notice];

    /// The directive keyword, without `@`.
    pub fn keyword(self) -> &'static str {
        match self {
            Level::Assert => "assert",
            Level::Warn => "warn",
            Level::Notice => "notice",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Op {
    Eq,
    Ne,
    Contains,
    NotContains,
    Lt,
    Le,
    Gt,
    Ge,
    Exists,
    NotExists,
}

/// A `# @assert SUBJECT OPERATOR [EXPECTED]` line (or `@warn`, `@notice`).
#[derive(Clone, Debug, PartialEq)]
pub struct Check {
    pub level: Level,
    /// The check as written after the keyword, for messages.
    pub text: String,
    /// One `{{...}}`, as written; `status` is `{{response.status}}`.
    subject: String,
    op: Op,
    /// Text to substitute and compare with, its quotes removed; `None` for
    /// `exists` and `!exists`.
    expected: Option<String>,
}

/// The outcome of one check against one response.
#[derive(Clone, Debug, PartialEq)]
pub struct CheckResult {
    pub level: Level,
    /// The check as written after the keyword.
    pub text: String,
    /// Why the check failed; `None` when it passed.
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
enum Part {
    /// A value in the JSON body, at `path` (as written, for messages).
    Body {
        path: String,
        steps: Vec<Step>,
    },
    Header(String),
    /// A cookie set by a `Set-Cookie` header, by name.
    Cookie(String),
    /// The status code; only in checks, as `{{response.status}}`.
    Status,
}

#[derive(Clone, Debug, PartialEq)]
enum Step {
    Key(String),
    Index(usize),
}

/// A filter applied to a `{{...}}` value with `|`, as in
/// `{{value | split("/", -1) | trim}}`.
#[derive(Clone, Debug, PartialEq)]
enum Filter {
    /// Part `index` (0-based; negative counts from the end) after splitting
    /// on `separator`.
    Split { separator: String, index: i64 },
    /// Leading and trailing whitespace removed.
    Trim,
    /// The text after this prefix, which must be present.
    Prefix(String),
    /// The text before this suffix, which must be present.
    Suffix(String),
}

/// A filter argument: a double-quoted string or an integer.
#[derive(Debug, PartialEq)]
enum Arg {
    Text(String),
    Int(i64),
}

/// A `# @foreach VAR in {{NAME.response.body.$.path}}` directive.
#[derive(Clone, Debug, PartialEq)]
pub struct Foreach {
    /// The loop variable, used as `{{VAR}}`, `{{VAR.key}}`, `{{VAR[N]}}`.
    pub var: String,
    /// The array to loop over (always a body reference).
    pub source: Reference,
}

/// The loop variable in scope while substituting a looping request, with the
/// current element (`None` when only checking).
#[derive(Clone, Copy)]
struct Item<'a> {
    var: &'a str,
    element: Option<&'a Value>,
}

/// What a `{{...}}` can see besides file variables, `$env` and other
/// requests' responses.
#[derive(Clone, Copy, Default)]
struct Scope<'a> {
    /// The loop variable of a looping request.
    item: Option<Item<'a>>,
    /// In a check, the request's own response, read by `{{response...}}`
    /// (`Some(None)` when only checking).
    own: Option<Option<&'a Captured>>,
}

/// The parts of a received response that references can read.
#[derive(Clone, Debug, Default)]
pub struct Captured {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Parses what follows `.response.` (or `response.` in a check); `own`
/// allows `status`, which only checks can read.
fn parse_part(rest: &str, own: bool) -> Result<Part> {
    if let Some(path) = rest.strip_prefix("body.") {
        return parse_path(path).map(|steps| Part::Body {
            path: path.to_string(),
            steps,
        });
    }
    if let Some(header) = rest.strip_prefix("headers.").filter(|h| !h.is_empty()) {
        return Ok(Part::Header(header.to_string()));
    }
    if let Some(cookie) = rest.strip_prefix("cookies.").filter(|c| !c.is_empty()) {
        return Ok(Part::Cookie(cookie.to_string()));
    }
    if own && rest == "status" {
        return Ok(Part::Status);
    }
    if own {
        bail!(
            "expected \"body.$...\", \"headers.NAME\", \"cookies.NAME\" or \"status\" after \"response.\""
        );
    }
    bail!("expected \"body.$...\", \"headers.NAME\" or \"cookies.NAME\" after \".response.\"")
}

impl Reference {
    /// Parses the trimmed text between `{{` and `}}`, or returns `None` when it
    /// is not a response reference. NAME is everything before `.response.`.
    fn parse(text: &str) -> Option<Result<Self>> {
        let (name, rest) = text.split_once(".response.")?;
        Some(
            parse_part(rest, false)
                .map(|part| Reference {
                    name: name.to_string(),
                    part,
                })
                .with_context(|| format!("invalid response reference {{{{{text}}}}}")),
        )
    }

    /// Parses a check's `response.…` reference to its own response.
    fn parse_own(text: &str) -> Option<Result<Self>> {
        let rest = text.strip_prefix("response.")?;
        Some(
            parse_part(rest, true)
                .map(|part| Reference {
                    name: String::new(),
                    part,
                })
                .with_context(|| format!("invalid response reference {{{{{text}}}}}")),
        )
    }

    /// `response of "NAME"`, or `response` for the checked request's own.
    fn whose(&self, what: &str) -> String {
        if self.name.is_empty() {
            what.to_string()
        } else {
            format!("{what} of \"{}\"", self.name)
        }
    }

    /// The referenced value as text: a JSON string without quotes, any other
    /// JSON value as compact JSON, the first value of the header (matched
    /// case-insensitively), or the value of the cookie (matched exactly; the
    /// last one wins when it is set more than once).
    pub fn extract(&self, response: &Captured) -> Result<String> {
        self.value(response).map(|value| json_text(&value))
    }

    /// The referenced value; a header or cookie value is a JSON string.
    fn value(&self, response: &Captured) -> Result<Value> {
        let whose = self.whose("response");
        match &self.part {
            Part::Status => Ok(Value::from(response.status)),
            Part::Header(header) => response
                .headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(header))
                .map(|(_, value)| Value::String(value.clone()))
                .with_context(|| format!("{whose} has no header {header}")),
            Part::Cookie(cookie) => response
                .headers
                .iter()
                .rev()
                .filter(|(n, _)| n.eq_ignore_ascii_case("set-cookie"))
                .filter_map(|(_, value)| set_cookie_pair(value))
                .find(|(n, _)| n == cookie)
                .map(|(_, value)| Value::String(value.to_string()))
                .with_context(|| format!("{whose} sets no cookie {cookie}")),
            Part::Body { path, steps } => {
                let body = self.whose("response body");
                let json: Value = serde_json::from_slice(&response.body)
                    .with_context(|| format!("{body} is not JSON"))?;
                walk(&json, steps)
                    .cloned()
                    .with_context(|| format!("{body} has no value at {path}"))
            }
        }
    }

    /// The reference as written, for messages.
    fn text(&self) -> String {
        let response = if self.name.is_empty() {
            "response".to_string()
        } else {
            format!("{}.response", self.name)
        };
        match &self.part {
            Part::Body { path, .. } => format!("{response}.body.{path}"),
            Part::Header(header) => format!("{response}.headers.{header}"),
            Part::Cookie(cookie) => format!("{response}.cookies.{cookie}"),
            Part::Status => format!("{response}.status"),
        }
    }
}

/// The name and value of a `Set-Cookie` header value (`name=value; attrs`),
/// trimmed, with one pair of surrounding double quotes removed from the value.
fn set_cookie_pair(header: &str) -> Option<(&str, &str)> {
    let pair = header.split(';').next()?;
    let (name, value) = pair.split_once('=')?;
    let value = value.trim();
    let value = value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or(value);
    Some((name.trim(), value))
}

/// The value at `steps` inside `value`, if there is one.
fn walk<'v>(mut value: &'v Value, steps: &[Step]) -> Option<&'v Value> {
    for step in steps {
        value = match step {
            Step::Key(key) => value.get(key.as_str()),
            Step::Index(index) => value.get(*index),
        }?;
    }
    Some(value)
}

/// A JSON string without quotes; any other value as compact JSON.
fn json_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Parses the text after `@foreach`: `VAR in {{NAME.response.body.$.path}}`.
fn parse_foreach(text: &str) -> Result<Foreach> {
    let invalid = || {
        anyhow::anyhow!(
            "invalid @foreach (expected \"@foreach VAR in {{{{NAME.response.body.$.path}}}}\"): @foreach {text}"
        )
    };
    let (var, rest) = text.split_once(char::is_whitespace).ok_or_else(invalid)?;
    let source = rest
        .trim_start()
        .strip_prefix("in")
        .filter(|r| r.starts_with(char::is_whitespace))
        .and_then(|r| r.trim().strip_prefix("{{"))
        .and_then(|r| r.strip_suffix("}}"))
        .ok_or_else(invalid)?
        .trim();
    if !var
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        bail!("invalid @foreach variable (use letters, digits, _ and -): {var}");
    }
    if split_pipes(source).len() > 1 {
        bail!("@foreach cannot use filters: {{{{{source}}}}}");
    }
    let Some(source) = Reference::parse(source) else {
        bail!(
            "@foreach must loop over a {{{{NAME.response.body.$.path}}}} reference: {{{{{source}}}}}"
        );
    };
    let source = source?;
    let kind = match source.part {
        Part::Body { .. } => None,
        Part::Header(_) => Some("header"),
        Part::Cookie(_) => Some("cookie"),
        Part::Status => Some("status"),
    };
    if let Some(kind) = kind {
        bail!(
            "@foreach must loop over a response body, not a {kind}: {{{{{}}}}}",
            source.text()
        );
    }
    Ok(Foreach {
        var: var.to_string(),
        source,
    })
}

/// Splits the text of a `{{...}}` on `|` outside double- and single-quoted
/// strings, trimming each part: the value first, then its filters.
fn split_pipes(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut quote = None;
    let mut escaped = false;
    let mut start = 0;
    for (i, c) in text.char_indices() {
        match quote {
            Some(_) if escaped => escaped = false,
            Some('"') if c == '\\' => escaped = true,
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c == '|' => {
                parts.push(text[start..i].trim());
                start = i + 1;
            }
            None => {}
        }
    }
    parts.push(text[start..].trim());
    parts
}

impl Filter {
    /// Parses one filter, as written between `|`s.
    fn parse(text: &str) -> Result<Self> {
        let (name, args) = match text.split_once('(') {
            Some((name, rest)) => {
                let args = rest
                    .trim_end()
                    .strip_suffix(')')
                    .with_context(|| format!("missing \")\" in filter {text}"))?;
                let args =
                    parse_args(args).with_context(|| format!("invalid arguments in {text}"))?;
                (name.trim(), args)
            }
            None => (text, Vec::new()),
        };
        let mut args = args.into_iter();
        let filter = match (name, args.next(), args.next(), args.next()) {
            ("split", Some(Arg::Text(separator)), Some(Arg::Int(index)), None)
                if !separator.is_empty() =>
            {
                Filter::Split { separator, index }
            }
            ("split", ..) => bail!("split takes a separator and an index: split(\"/\", -1)"),
            ("trim", None, ..) => Filter::Trim,
            ("trim", ..) => bail!("trim takes no arguments"),
            ("prefix", Some(Arg::Text(prefix)), None, _) => Filter::Prefix(prefix),
            ("prefix", ..) => bail!("prefix takes one string: prefix(\"Bearer \")"),
            ("suffix", Some(Arg::Text(suffix)), None, _) => Filter::Suffix(suffix),
            ("suffix", ..) => bail!("suffix takes one string: suffix(\".json\")"),
            _ => bail!("unknown filter \"{name}\" (expected split, trim, prefix or suffix)"),
        };
        Ok(filter)
    }

    /// The filter as written, for messages.
    fn text(&self) -> String {
        match self {
            Filter::Split { separator, index } => format!("split({separator:?}, {index})"),
            Filter::Trim => "trim".to_string(),
            Filter::Prefix(prefix) => format!("prefix({prefix:?})"),
            Filter::Suffix(suffix) => format!("suffix({suffix:?})"),
        }
    }

    fn apply(&self, value: &str) -> Result<String> {
        let result = match self {
            Filter::Split { separator, index } => {
                let parts: Vec<_> = value.split(separator.as_str()).collect();
                let position = if *index < 0 {
                    parts.len() as i64 + index
                } else {
                    *index
                };
                usize::try_from(position)
                    .ok()
                    .and_then(|position| parts.get(position))
                    .map(|part| part.to_string())
                    .with_context(|| {
                        let count = parts.len();
                        let noun = if count == 1 { "part" } else { "parts" };
                        format!("{value:?} gives {count} {noun}")
                    })
            }
            Filter::Trim => Ok(value.trim().to_string()),
            Filter::Prefix(prefix) => value
                .strip_prefix(prefix.as_str())
                .map(str::to_string)
                .with_context(|| format!("{value:?} does not start with {prefix:?}")),
            Filter::Suffix(suffix) => value
                .strip_suffix(suffix.as_str())
                .map(str::to_string)
                .with_context(|| format!("{value:?} does not end with {suffix:?}")),
        };
        result.with_context(|| self.text())
    }
}

/// Parses comma-separated filter arguments: double-quoted strings (with `\"`
/// and `\\` escapes) and integers.
fn parse_args(text: &str) -> Result<Vec<Arg>> {
    let mut args = Vec::new();
    let mut rest = text.trim();
    if rest.is_empty() {
        return Ok(args);
    }
    loop {
        if let Some(after) = rest.strip_prefix('"') {
            let (value, after) = parse_quoted(after)?;
            args.push(Arg::Text(value));
            rest = after.trim_start();
        } else {
            let end = rest.find(',').unwrap_or(rest.len());
            let number = rest[..end].trim();
            let number = number
                .parse()
                .with_context(|| format!("expected a \"string\" or an integer: {number}"))?;
            args.push(Arg::Int(number));
            rest = &rest[end..];
        }
        if rest.is_empty() {
            return Ok(args);
        }
        rest = rest
            .strip_prefix(',')
            .context("expected \",\" between arguments")?
            .trim_start();
    }
}

/// Reads a double-quoted string (with `\"` and `\\` escapes) whose opening
/// quote has been stripped, returning its value and the text after the
/// closing quote.
fn parse_quoted(after: &str) -> Result<(String, &str)> {
    let mut value = String::new();
    let mut chars = after.char_indices();
    loop {
        match chars.next() {
            Some((_, '\\')) => match chars.next() {
                Some((_, c @ ('"' | '\\'))) => value.push(c),
                _ => bail!("only \\\" and \\\\ escapes are supported"),
            },
            Some((i, '"')) => return Ok((value, &after[i + 1..])),
            Some((_, c)) => value.push(c),
            None => bail!("unterminated string"),
        }
    }
}

/// Parses the text after `@assert`, `@warn` or `@notice`:
/// `SUBJECT OPERATOR [EXPECTED]`.
fn parse_check(level: Level, text: &str) -> Result<Check> {
    let keyword = level.keyword();
    let invalid = |why: String| anyhow::anyhow!("invalid @{keyword} ({why}): @{keyword} {text}");
    let (subject, rest) = if let Some(after) = text.strip_prefix("{{") {
        let end = after
            .find("}}")
            .ok_or_else(|| invalid("unterminated \"{{\"".to_string()))?;
        (text[..end + 4].to_string(), &after[end + 2..])
    } else if let Some(rest) = text
        .strip_prefix("status")
        .filter(|r| r.is_empty() || r.starts_with(char::is_whitespace))
    {
        ("{{response.status}}".to_string(), rest)
    } else {
        return Err(invalid("expected status or {{...}} first".to_string()));
    };
    let rest = rest.trim();
    let (op_text, expected) = rest
        .split_once(char::is_whitespace)
        .map_or((rest, ""), |(op, expected)| (op, expected.trim()));
    let op = match op_text {
        "==" => Op::Eq,
        "!=" => Op::Ne,
        "contains" => Op::Contains,
        "!contains" => Op::NotContains,
        "<" => Op::Lt,
        "<=" => Op::Le,
        ">" => Op::Gt,
        ">=" => Op::Ge,
        "exists" => Op::Exists,
        "!exists" => Op::NotExists,
        "" => return Err(invalid("missing operator".to_string())),
        other => {
            return Err(invalid(format!(
                "unknown operator \"{other}\"; expected ==, !=, contains, !contains, <, <=, >, >=, exists or !exists"
            )));
        }
    };
    let expected = match (
        matches!(op, Op::Exists | Op::NotExists),
        expected.is_empty(),
    ) {
        (true, true) => None,
        (true, false) => return Err(invalid(format!("{op_text} takes no value"))),
        (false, true) => return Err(invalid(format!("{op_text} needs a value"))),
        (false, false) => Some(match expected.strip_prefix('"') {
            Some(after) => {
                let (value, rest) = parse_quoted(after).map_err(|e| invalid(e.to_string()))?;
                if !rest.trim().is_empty() {
                    return Err(invalid(format!(
                        "unexpected text after the closing quote: {}",
                        rest.trim()
                    )));
                }
                value
            }
            None => expected.to_string(),
        }),
    };
    Ok(Check {
        level,
        text: text.to_string(),
        subject,
        op,
        expected,
    })
}

impl Check {
    /// Compares the subject with the expected value; `eval` substitutes a
    /// `{{...}}` text. The error says why the check failed.
    fn evaluate(&self, eval: &mut dyn FnMut(&str) -> Result<String>) -> Result<()> {
        let subject = eval(&self.subject);
        let got = |value: &str| anyhow::anyhow!("got {value:?}");
        let actual = match (self.op, subject) {
            (Op::Exists, subject) => return subject.map(drop),
            (Op::NotExists, Ok(value)) => return Err(got(&value)),
            (Op::NotExists, Err(_)) => return Ok(()),
            (_, subject) => subject?,
        };
        let expected = eval(self.expected.as_deref().unwrap_or_default())?;
        let pass = match self.op {
            Op::Eq => actual == expected,
            Op::Ne => actual != expected,
            Op::Contains => actual.contains(&expected),
            Op::NotContains => !actual.contains(&expected),
            op => {
                let (a, e) = (number(&actual)?, number(&expected)?);
                match op {
                    Op::Lt => a < e,
                    Op::Le => a <= e,
                    Op::Gt => a > e,
                    _ => a >= e,
                }
            }
        };
        if pass { Ok(()) } else { Err(got(&actual)) }
    }
}

/// A finite number, for `<`, `<=`, `>` and `>=`.
fn number(text: &str) -> Result<f64> {
    text.trim()
        .parse::<f64>()
        .ok()
        .filter(|n| n.is_finite())
        .with_context(|| format!("not a number: {text:?}"))
}

/// The path after the loop variable in `{{VAR...}}`, or `None` when `name`
/// does not use the loop variable.
fn loop_path<'a>(name: &'a str, var: &str) -> Option<&'a str> {
    name.strip_prefix(var)
        .filter(|rest| rest.is_empty() || rest.starts_with(['.', '[']))
}

/// Parses the JSONPath subset `$`, `.key`, `['key']` and `[N]`.
fn parse_path(path: &str) -> Result<Vec<Step>> {
    let unsupported =
        || anyhow::anyhow!("unsupported JSONPath (expected $, .key, ['key'] or [N]): {path}");
    let mut rest = path.strip_prefix('$').ok_or_else(unsupported)?;
    let mut steps = Vec::new();
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("['") {
            let end = after.find("']").ok_or_else(unsupported)?;
            steps.push(Step::Key(after[..end].to_string()));
            rest = &after[end + 2..];
        } else if let Some(after) = rest.strip_prefix('[') {
            let end = after.find(']').ok_or_else(unsupported)?;
            let index = &after[..end];
            if index.is_empty() || !index.bytes().all(|b| b.is_ascii_digit()) {
                return Err(unsupported());
            }
            steps.push(Step::Index(index.parse().map_err(|_| unsupported())?));
            rest = &after[end + 1..];
        } else if let Some(after) = rest.strip_prefix('.') {
            let end = after.find(['.', '[']).unwrap_or(after.len());
            let key = &after[..end];
            if key.is_empty() || key == "*" {
                return Err(unsupported());
            }
            steps.push(Step::Key(key.to_string()));
            rest = &after[end..];
        } else {
            return Err(unsupported());
        }
    }
    Ok(steps)
}

pub fn parse(contents: &str) -> Result<HttpFile> {
    let mut file = HttpFile {
        variables: Vec::new(),
        requests: Vec::new(),
    };
    let mut block = Vec::new();
    let mut name = None;
    let mut start = 1;
    let mut last = 0;
    for (index, line) in contents.lines().enumerate() {
        last = index + 1;
        if let Some(rest) = line.strip_prefix("###") {
            file.parse_block(name.take(), (start, last - 1), &block)?;
            block.clear();
            name = clean_name(rest);
            start = last;
        } else {
            block.push((last, line));
        }
    }
    file.parse_block(name, (start, last), &block)?;
    Ok(file)
}

impl HttpFile {
    /// Parses the lines between two `###` separators. A block without a
    /// request line (e.g. only variables or comments) adds no request.
    fn parse_block(
        &mut self,
        mut name: Option<String>,
        (start_line, end_line): (usize, usize),
        lines: &[(usize, &str)],
    ) -> Result<()> {
        let mut lines = lines.iter();
        let mut foreach = None;
        let mut checks = Vec::new();

        // Variables, comments, `# @name`, `# @foreach` and checks, up to the
        // request line.
        let (line_no, request_line) = loop {
            let Some(&(line_no, line)) = lines.next() else {
                return Ok(());
            };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some(directive) = foreach_directive(line) {
                let directive = directive.with_context(|| format!("line {line_no}"))?;
                if foreach.replace(directive).is_some() {
                    bail!("line {line_no}: more than one @foreach in a request");
                }
            } else if let Some((_, check)) = check_directive(line) {
                checks.push(check.with_context(|| format!("line {line_no}"))?);
            } else if let Some(directive) = name_directive(line) {
                name = Some(directive);
            } else if is_comment(line) {
                continue;
            } else if let Some(variable) = line.strip_prefix('@') {
                let Some((var, value)) = variable.split_once('=') else {
                    bail!("line {line_no}: invalid variable (expected \"@name = value\"): {line}");
                };
                self.variables
                    .push((var.trim().to_string(), value.trim().to_string()));
            } else {
                break (line_no, line);
            }
        };

        let mut parts = split_request_line(request_line);
        if parts.len() > 1 && parts.last().is_some_and(|p| p.starts_with("HTTP/")) {
            parts.pop();
        }
        let (method, url) = match parts[..] {
            [url] => ("GET", url),
            [method, url] => (method, url),
            _ => bail!(
                "line {line_no}: invalid request line (expected \"METHOD URL [HTTP-version]\"): {request_line}"
            ),
        };

        // Headers up to the first blank line, then the body.
        let mut headers = Vec::new();
        let mut body = Vec::new();
        let mut in_body = false;
        for &(line_no, line) in lines {
            if in_body {
                body.push(line);
                continue;
            }
            let line = line.trim();
            if line.is_empty() {
                in_body = true;
            } else if let Some((level, _)) = check_directive(line) {
                bail!(
                    "line {line_no}: @{} must come before the request line",
                    level.keyword()
                );
            } else if !is_comment(line) {
                let Some((header, value)) = line.split_once(':') else {
                    bail!("line {line_no}: invalid header (expected \"Name: Value\"): {line}");
                };
                headers.push((header.trim().to_string(), value.trim().to_string()));
            }
        }
        while body.last().is_some_and(|l| l.trim().is_empty()) {
            body.pop();
        }

        self.requests.push(Request {
            name,
            method: method.to_ascii_uppercase(),
            url: url.to_string(),
            headers,
            body: (!body.is_empty()).then(|| body.join("\n")),
            foreach,
            checks,
            start_line,
            end_line,
        });
        Ok(())
    }

    /// Substitutes variables in the request's URL, headers and body, taking
    /// response references from `responses` (keyed by request name) and, for
    /// a looping request, the loop variable from `element`.
    pub fn resolve(
        &self,
        request: &Request,
        responses: &HashMap<String, Captured>,
        element: Option<&Value>,
    ) -> Result<Request> {
        let item = match (&request.foreach, element) {
            (Some(foreach), Some(element)) => Some(Item {
                var: &foreach.var,
                element: Some(element),
            }),
            (None, None) => None,
            (Some(_), None) => bail!("a looping request needs an element to resolve"),
            (None, Some(_)) => bail!("only a looping request takes an element"),
        };
        self.substitute_request(request, item, &mut |reference| {
            let response = responses
                .get(&reference.name)
                .with_context(|| format!("no response from \"{}\" yet", reference.name))?;
            reference.extract(response).map(Some)
        })
    }

    /// The distinct response references in the request, including those
    /// reached through file variables and a `@foreach` source (first), in
    /// order of appearance. Also checks the request's variables, like
    /// [`HttpFile::resolve`].
    pub fn references(&self, request: &Request) -> Result<Vec<Reference>> {
        let mut found: Vec<Reference> = Vec::new();
        let mut item = None;
        if let Some(foreach) = &request.foreach {
            if self.variables.iter().any(|(name, _)| *name == foreach.var) {
                bail!(
                    "@foreach variable \"{}\" has the same name as a file variable",
                    foreach.var
                );
            }
            found.push(foreach.source.clone());
            item = Some(Item {
                var: &foreach.var,
                element: None,
            });
        }
        let mut collect = |reference: &Reference| -> Result<Option<String>> {
            if !found.contains(reference) {
                found.push(reference.clone());
            }
            Ok(None)
        };
        self.substitute_request(request, item, &mut collect)?;
        // A check's `{{response...}}` is its own response, not a dependency.
        let scope = Scope {
            item,
            own: Some(None),
        };
        for check in &request.checks {
            for text in [Some(&check.subject), check.expected.as_ref()]
                .into_iter()
                .flatten()
            {
                self.substitute(text, self.variables.len(), scope, &mut collect)
                    .with_context(|| format!("@{} {}", check.level.keyword(), check.text))?;
            }
        }
        Ok(found)
    }

    /// Evaluates the request's checks against `response`, its own response,
    /// with other requests' responses from `responses` and, for a looping
    /// request, the loop variable from `element`.
    pub fn check(
        &self,
        request: &Request,
        response: &Captured,
        responses: &HashMap<String, Captured>,
        element: Option<&Value>,
    ) -> Vec<CheckResult> {
        let scope = Scope {
            item: request
                .foreach
                .as_ref()
                .zip(element)
                .map(|(foreach, element)| Item {
                    var: &foreach.var,
                    element: Some(element),
                }),
            own: Some(Some(response)),
        };
        let mut on_reference = |reference: &Reference| -> Result<Option<String>> {
            let response = responses
                .get(&reference.name)
                .with_context(|| format!("no response from \"{}\" yet", reference.name))?;
            reference.extract(response).map(Some)
        };
        request
            .checks
            .iter()
            .map(|check| {
                let mut eval = |text: &str| {
                    self.substitute(text, self.variables.len(), scope, &mut on_reference)
                        .map(Option::unwrap_or_default)
                };
                CheckResult {
                    level: check.level,
                    text: check.text.clone(),
                    error: check.evaluate(&mut eval).err().map(|e| format!("{e:#}")),
                }
            })
            .collect()
    }

    /// For a looping request, the elements of its `@foreach` array, read from
    /// `responses`; `None` for a request that does not loop.
    pub fn loop_items(
        &self,
        request: &Request,
        responses: &HashMap<String, Captured>,
    ) -> Result<Option<Vec<Value>>> {
        let Some(foreach) = &request.foreach else {
            return Ok(None);
        };
        let source = &foreach.source;
        let response = responses
            .get(&source.name)
            .with_context(|| format!("no response from \"{}\" yet", source.name))?;
        match source.value(response)? {
            Value::Array(items) => Ok(Some(items)),
            _ => bail!("@foreach: {{{{{}}}}} is not an array", source.text()),
        }
    }

    fn substitute_request(
        &self,
        request: &Request,
        item: Option<Item>,
        on_reference: OnReference,
    ) -> Result<Request> {
        let all = self.variables.len();
        let scope = Scope { item, own: None };
        // While only checking, unknown values are left empty.
        let mut substitute = |text| {
            Ok::<_, anyhow::Error>(
                self.substitute(text, all, scope, on_reference)?
                    .unwrap_or_default(),
            )
        };
        let url = substitute(&request.url)?;
        let mut headers = Vec::with_capacity(request.headers.len());
        for (name, value) in &request.headers {
            headers.push((substitute(name)?, substitute(value)?));
        }
        let body = match &request.body {
            Some(body) => Some(substitute(body)?),
            None => None,
        };
        Ok(Request {
            name: request.name.clone(),
            method: request.method.clone(),
            url,
            headers,
            body,
            foreach: request.foreach.clone(),
            checks: request.checks.clone(),
            start_line: request.start_line,
            end_line: request.end_line,
        })
    }

    /// Replaces each `{{...}}` in `text`, seeing only the first `visible`
    /// file variables (so a variable can only reference earlier ones) and
    /// what `scope` adds. `None` when a value is not known
    /// because only the request is being checked.
    fn substitute(
        &self,
        text: &str,
        visible: usize,
        scope: Scope,
        on_reference: OnReference,
    ) -> Result<Option<String>> {
        let mut out = String::with_capacity(text.len());
        let mut known = true;
        let mut rest = text;
        while let Some(start) = rest.find("{{") {
            out.push_str(&rest[..start]);
            let after = &rest[start + 2..];
            let Some(end) = after.find("}}") else {
                bail!("unterminated \"{{{{\" in: {text}");
            };
            match self.lookup(after[..end].trim(), visible, scope, on_reference)? {
                Some(value) => out.push_str(&value),
                None => known = false,
            }
            rest = &after[end + 2..];
        }
        out.push_str(rest);
        Ok(known.then_some(out))
    }

    /// The value of the text inside `{{...}}`, with its filters applied.
    fn lookup(
        &self,
        text: &str,
        visible: usize,
        scope: Scope,
        on_reference: OnReference,
    ) -> Result<Option<String>> {
        let mut parts = split_pipes(text).into_iter();
        let name = parts.next().unwrap_or_default();
        let filters = parts
            .map(Filter::parse)
            .collect::<Result<Vec<_>>>()
            .with_context(|| format!("invalid filter in {{{{{text}}}}}"))?;
        let Some(mut value) = self.lookup_value(name, visible, scope, on_reference)? else {
            return Ok(None);
        };
        for filter in &filters {
            value = filter
                .apply(&value)
                .with_context(|| format!("{{{{{text}}}}}"))?;
        }
        Ok(Some(value))
    }

    /// The value of a variable, loop variable or response reference.
    fn lookup_value(
        &self,
        name: &str,
        visible: usize,
        scope: Scope,
        on_reference: OnReference,
    ) -> Result<Option<String>> {
        if let Some(dynamic) = name.strip_prefix('$') {
            let Some(var) = dynamic
                .strip_prefix("env")
                .filter(|v| v.starts_with(char::is_whitespace))
            else {
                bail!("unsupported dynamic variable: {{{{{name}}}}}");
            };
            let var = var.trim();
            return std::env::var(var)
                .map(Some)
                .with_context(|| format!("environment variable not set: {var}"));
        }
        // In a check, `{{response...}}` is the request's own response.
        if let Some(own) = scope.own
            && let Some(reference) = Reference::parse_own(name)
        {
            let reference = reference?;
            return match own {
                Some(response) => reference.extract(response).map(Some),
                None => Ok(None),
            };
        }
        // The loop variable shadows requests with the same name.
        if let Some((item, path)) = scope
            .item
            .and_then(|item| Some((item, loop_path(name, item.var)?)))
        {
            let steps = parse_path(&format!("${path}"))
                .with_context(|| format!("invalid loop variable reference {{{{{name}}}}}"))?;
            let Some(element) = item.element else {
                return Ok(None);
            };
            return walk(element, &steps)
                .map(|value| Some(json_text(value)))
                .with_context(|| format!("current element has no value at {{{{{name}}}}}"));
        }
        if let Some(reference) = Reference::parse(name) {
            return on_reference(&reference?);
        }
        // A redefined variable takes its latest earlier definition.
        let index = self.variables[..visible]
            .iter()
            .rposition(|(n, _)| n == name)
            .with_context(|| format!("undefined variable: {name}"))?;
        // The loop variable and a check's own response are not visible
        // inside file variables.
        self.substitute(
            &self.variables[index].1,
            index,
            Scope::default(),
            on_reference,
        )
    }
}

/// Supplies the value of each response reference met while substituting, or
/// `None` while only checking.
type OnReference<'a> = &'a mut dyn FnMut(&Reference) -> Result<Option<String>>;

/// Splits on whitespace, except inside `{{...}}` (as in `{{$env NAME}}`).
fn split_request_line(line: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = None;
    let mut depth = 0;
    let mut chars = line.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let next = chars.peek().map(|&(_, n)| n);
        if c == '{' && next == Some('{') {
            depth += 1;
            chars.next();
        } else if c == '}' && next == Some('}') && depth > 0 {
            depth -= 1;
            chars.next();
        } else if c.is_whitespace() && depth == 0 {
            if let Some(s) = start.take() {
                parts.push(&line[s..i]);
            }
            continue;
        }
        start.get_or_insert(i);
    }
    if let Some(s) = start {
        parts.push(&line[s..]);
    }
    parts
}

fn is_comment(line: &str) -> bool {
    line.starts_with('#') || line.starts_with("//")
}

/// The parsed `# @foreach ...` or `// @foreach ...` line, or `None` when the
/// line is not a `@foreach` directive.
fn foreach_directive(line: &str) -> Option<Result<Foreach>> {
    let rest = line.strip_prefix('#').or_else(|| line.strip_prefix("//"))?;
    let rest = rest.trim_start().strip_prefix("@foreach")?;
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    Some(parse_foreach(rest.trim()))
}

/// The level and parsed check of a `# @assert ...`, `# @warn ...` or
/// `# @notice ...` line (or `//`), or `None` when the line is not a check.
fn check_directive(line: &str) -> Option<(Level, Result<Check>)> {
    let rest = line.strip_prefix('#').or_else(|| line.strip_prefix("//"))?;
    let rest = rest.trim_start().strip_prefix('@')?;
    Level::ALL.into_iter().find_map(|level| {
        let text = rest.strip_prefix(level.keyword())?;
        if !text.is_empty() && !text.starts_with(char::is_whitespace) {
            return None;
        }
        Some((level, parse_check(level, text.trim())))
    })
}

/// The name from a `# @name NAME` or `// @name NAME` line.
fn name_directive(line: &str) -> Option<String> {
    let rest = line.strip_prefix('#').or_else(|| line.strip_prefix("//"))?;
    let name = rest.trim_start().strip_prefix("@name")?;
    if !name.starts_with(char::is_whitespace) {
        return None;
    }
    clean_name(name)
}

/// Trims a request name and drops a trailing ` // comment`. The `//` must
/// follow whitespace, so a name like `Fetch http://a` is kept whole.
fn clean_name(name: &str) -> Option<String> {
    let name = name
        .char_indices()
        .find(|&(i, c)| c.is_whitespace() && name[i..].trim_start().starts_with("//"))
        .map_or(name, |(i, _)| &name[..i]);
    Some(name.trim().to_string()).filter(|n| !n.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(
        name: Option<&str>,
        method: &str,
        url: &str,
        (start_line, end_line): (usize, usize),
    ) -> Request {
        Request {
            name: name.map(Into::into),
            method: method.into(),
            url: url.into(),
            headers: Vec::new(),
            body: None,
            foreach: None,
            checks: Vec::new(),
            start_line,
            end_line,
        }
    }

    fn resolved(contents: &str) -> Result<Request> {
        let file = parse(contents).unwrap();
        file.resolve(&file.requests[0], &HashMap::new(), None)
    }

    fn reference(text: &str) -> Result<Reference> {
        Reference::parse(text).expect("is a reference")
    }

    fn captured(body: &str) -> Captured {
        Captured {
            status: 200,
            headers: vec![
                ("Location".into(), "/next".into()),
                ("x-dup".into(), "first".into()),
                ("X-Dup".into(), "second".into()),
                ("Set-Cookie".into(), "session=old; Path=/".into()),
                ("set-cookie".into(), " theme = \"dark\" ; Secure".into()),
                (
                    "set-cookie".into(),
                    "session=abc123; Path=/; HttpOnly".into(),
                ),
                ("set-cookie".into(), "empty=; Max-Age=0".into()),
                ("set-cookie".into(), "invalid".into()),
            ],
            body: body.as_bytes().to_vec(),
        }
    }

    const BODY: &str = r#"{"token":"t0k","n":1.5,"ok":true,"nil":null,"items":[{"id":7},{"id":8}],"a.b":{"c d":"x"},"obj":{"k":[1,2]}}"#;

    #[test]
    fn extracts_json_body_values() {
        let response = captured(BODY);
        let cases = [
            ("login.response.body.$.token", "t0k"),
            ("login.response.body.$.n", "1.5"),
            ("login.response.body.$.ok", "true"),
            ("login.response.body.$.nil", "null"),
            ("login.response.body.$.items[1].id", "8"),
            ("login.response.body.$['items'][0]['id']", "7"),
            ("login.response.body.$['a.b']['c d']", "x"),
            ("login.response.body.$.obj", r#"{"k":[1,2]}"#),
            ("login.response.body.$", BODY),
        ];
        for (text, expected) in cases {
            assert_eq!(
                reference(text).unwrap().extract(&response).unwrap(),
                expected,
                "{text}"
            );
        }
    }

    #[test]
    fn extracts_first_header_case_insensitively() {
        let response = captured("");
        let location = reference("login.response.headers.location").unwrap();
        assert_eq!(location.extract(&response).unwrap(), "/next");
        let dup = reference("login.response.headers.X-DUP").unwrap();
        assert_eq!(dup.extract(&response).unwrap(), "first");
    }

    #[test]
    fn extracts_cookie_values() {
        let response = captured("");
        let cases = [
            ("login.response.cookies.session", "abc123"),
            ("login.response.cookies.theme", "dark"),
            ("login.response.cookies.empty", ""),
        ];
        for (text, expected) in cases {
            assert_eq!(
                reference(text).unwrap().extract(&response).unwrap(),
                expected,
                "{text}"
            );
        }
    }

    #[test]
    fn name_is_everything_before_response() {
        let r = reference("create item.v2.response.headers.Location").unwrap();
        assert_eq!(r.name, "create item.v2");
    }

    #[test]
    fn missing_values_are_errors() {
        let response = captured(BODY);
        let err = |text: &str, response: &Captured| {
            reference(text)
                .unwrap()
                .extract(response)
                .unwrap_err()
                .to_string()
        };
        assert_eq!(
            err("a.response.body.$.items[5]", &response),
            "response body of \"a\" has no value at $.items[5]"
        );
        assert_eq!(
            err("a.response.body.$.token.x", &response),
            "response body of \"a\" has no value at $.token.x"
        );
        assert_eq!(
            err("a.response.headers.Nope", &response),
            "response of \"a\" has no header Nope"
        );
        // Cookie names are case-sensitive.
        assert_eq!(
            err("a.response.cookies.Session", &response),
            "response of \"a\" sets no cookie Session"
        );
        assert_eq!(
            err("a.response.body.$.x", &captured("not json")),
            "response body of \"a\" is not JSON"
        );
    }

    #[test]
    fn unsupported_references_are_errors() {
        for text in [
            "a.response.body.token",
            "a.response.body.$..token",
            "a.response.body.$.*",
            "a.response.body.$.items[*]",
            "a.response.body.$.items[+1]",
            "a.response.body.$[?(@.id)]",
            "a.response.body.$['open",
            "a.response.headers.",
            "a.response.cookies.",
            "a.response.cookie.session",
            "a.response.status",
        ] {
            let err = format!("{:#}", reference(text).unwrap_err());
            assert!(
                err.starts_with(&format!("invalid response reference {{{{{text}}}}}")),
                "{err}"
            );
        }
    }

    #[test]
    fn collects_references_including_through_variables() {
        let file = parse(
            "@token = {{login.response.body.$.token}}\n### me\nGET http://a/{{other.response.headers.Location}}\nAuthorization: Bearer {{token}}\n\n{{ login.response.body.$.token }}\n",
        )
        .unwrap();
        let names: Vec<_> = file
            .references(&file.requests[0])
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect();
        assert_eq!(names, ["other", "login"]);
    }

    #[test]
    fn resolves_references_from_responses() {
        let file = parse(
            "@token = {{login.response.body.$.token}}\nGET http://a{{login.response.headers.Location}}\nAuthorization: Bearer {{token}}\n",
        )
        .unwrap();
        let responses = HashMap::from([("login".to_string(), captured(BODY))]);
        let r = file.resolve(&file.requests[0], &responses, None).unwrap();
        assert_eq!(r.url, "http://a/next");
        assert_eq!(r.headers, [("Authorization".into(), "Bearer t0k".into())]);

        let err = file
            .resolve(&file.requests[0], &HashMap::new(), None)
            .unwrap_err();
        assert_eq!(err.to_string(), "no response from \"login\" yet");
    }

    const LOOP: &str = "### each\n# @foreach item in {{login.response.body.$.items}}\nGET http://a/{{item.id}}?all={{item}}\nX-Name: {{ item['id'] }}\n";

    #[test]
    fn parses_foreach_directive() {
        let file = parse(LOOP).unwrap();
        let foreach = file.requests[0].foreach.as_ref().unwrap();
        assert_eq!(foreach.var, "item");
        assert_eq!(foreach.source.name, "login");

        let file = parse("// @foreach x in {{a.response.body.$}}\nGET http://a\n").unwrap();
        assert_eq!(file.requests[0].foreach.as_ref().unwrap().var, "x");

        // `@foreachx` is just a comment.
        let file = parse("# @foreachx\nGET http://a\n").unwrap();
        assert_eq!(file.requests[0].foreach, None);
    }

    #[test]
    fn invalid_foreach_directives() {
        for (contents, expected) in [
            ("# @foreach\nGET http://a\n", "line 1: invalid @foreach"),
            (
                "# @foreach x {{a.response.body.$}}\nGET http://a\n",
                "line 1: invalid @foreach",
            ),
            (
                "# @foreach x in a.response.body.$\nGET http://a\n",
                "line 1: invalid @foreach",
            ),
            (
                "# @foreach x.y in {{a.response.body.$}}\nGET http://a\n",
                "line 1: invalid @foreach variable",
            ),
            (
                "# @foreach x in {{list}}\nGET http://a\n",
                "line 1: @foreach must loop over a {{NAME.response.body.$.path}} reference",
            ),
            (
                "# @foreach x in {{a.response.headers.Link}}\nGET http://a\n",
                "line 1: @foreach must loop over a response body, not a header",
            ),
            (
                "# @foreach x in {{a.response.cookies.ids}}\nGET http://a\n",
                "line 1: @foreach must loop over a response body, not a cookie",
            ),
            (
                "# @foreach x in {{a.response.body.$.ids | trim}}\nGET http://a\n",
                "line 1: @foreach cannot use filters",
            ),
            (
                "# @foreach x in {{a.response.body.$.*}}\nGET http://a\n",
                "line 1: invalid response reference",
            ),
            (
                "# @foreach x in {{a.response.body.$}}\n# @foreach y in {{a.response.body.$}}\nGET http://a\n",
                "line 2: more than one @foreach in a request",
            ),
        ] {
            let err = format!("{:#}", parse(contents).err().unwrap());
            assert!(err.starts_with(expected), "{contents}: {err}");
        }
    }

    #[test]
    fn loop_references_include_the_source_first() {
        let file = parse(&format!(
            "{LOOP}X-Other: {{{{other.response.headers.A}}}}\n"
        ))
        .unwrap();
        let names: Vec<_> = file
            .references(&file.requests[0])
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect();
        assert_eq!(names, ["login", "other"]);
    }

    #[test]
    fn loop_variable_checks() {
        let clash = parse(&format!("@item = 1\n{LOOP}")).unwrap();
        let err = clash.references(&clash.requests[0]).unwrap_err();
        assert_eq!(
            err.to_string(),
            "@foreach variable \"item\" has the same name as a file variable"
        );

        let bad =
            parse("# @foreach item in {{a.response.body.$}}\nGET http://a/{{item..x}}\n").unwrap();
        let err = bad.references(&bad.requests[0]).unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid loop variable reference {{item..x}}"
        );

        // Only visible in its own request, not inside file variables.
        let hidden =
            parse("@v = {{item}}\n# @foreach item in {{a.response.body.$}}\nGET http://a/{{v}}\n")
                .unwrap();
        let err = hidden.references(&hidden.requests[0]).unwrap_err();
        assert_eq!(err.to_string(), "undefined variable: item");
    }

    #[test]
    fn loop_items_and_elements() {
        let file = parse(LOOP).unwrap();
        let request = &file.requests[0];
        let responses = HashMap::from([("login".to_string(), captured(BODY))]);
        let items = file.loop_items(request, &responses).unwrap().unwrap();
        assert_eq!(items.len(), 2);

        let r = file.resolve(request, &responses, Some(&items[1])).unwrap();
        assert_eq!(r.url, r#"http://a/8?all={"id":8}"#);
        assert_eq!(r.headers, [("X-Name".into(), "8".into())]);

        let missing = serde_json::json!({ "name": "no id" });
        let err = file
            .resolve(request, &responses, Some(&missing))
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "current element has no value at {{item.id}}"
        );

        let not_array =
            parse("# @foreach x in {{login.response.body.$.token}}\nGET http://a\n").unwrap();
        let err = not_array
            .loop_items(&not_array.requests[0], &responses)
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "@foreach: {{login.response.body.$.token}} is not an array"
        );

        let plain = parse("GET http://a\n").unwrap();
        assert_eq!(
            plain.loop_items(&plain.requests[0], &responses).unwrap(),
            None
        );
    }

    /// Resolves `GET http://a/{{expr}}` against `captured(BODY)` as "login",
    /// returning the substituted path.
    fn filtered(variables: &str, expr: &str) -> Result<String> {
        let file = parse(&format!("{variables}GET http://a/{{{{{expr}}}}}\n"))?;
        let responses = HashMap::from([("login".to_string(), captured(BODY))]);
        file.references(&file.requests[0])?;
        let r = file.resolve(&file.requests[0], &responses, None)?;
        Ok(r.url.strip_prefix("http://a/").unwrap().to_string())
    }

    #[test]
    fn filters_transform_values() {
        let cases = [
            (r#"login.response.headers.Location | split("/", 1)"#, "next"),
            (
                r#"login.response.headers.Location | split("/", -1)"#,
                "next",
            ),
            (r#"login.response.headers.Location | split("/", 0)"#, ""),
            (r#"login.response.headers.Location | prefix("/")"#, "next"),
            (r#"login.response.headers.Location | suffix("xt")"#, "/ne"),
            (
                r#"login.response.headers.Location|prefix("/")|suffix("t")"#,
                "nex",
            ),
            (r#"login.response.cookies.session | split("1", -1)"#, "23"),
            (r#"login.response.body.$.token | split("0", 0)"#, "t"),
            (r#"login.response.body.$['a.b'] | split("\"", 1)"#, "c d"),
            (r#"v | trim"#, "a|b, \"c\""),
            (r#"v | trim()"#, "a|b, \"c\""),
            (r#"v | split("|", 0)"#, "a"),
            (
                r#"v | split(", ", -1) | trim | prefix("\"") | suffix("\"")"#,
                "c",
            ),
            (r#"v|split("\\", 0)"#, "a|b, \"c\""),
            (r#"from_v | prefix("x-")"#, "a"),
        ];
        let variables = "@v =  a|b, \"c\" \n@from_v = {{v | trim | split(\"|\", 0) | prefix(\"a\") | suffix(\"\")}}x-a\n";
        for (expr, expected) in cases {
            assert_eq!(filtered(variables, expr).unwrap(), expected, "{expr}");
        }
    }

    #[test]
    fn filters_apply_to_loop_and_env_values() {
        // `}}` inside a filter argument ends the `{{...}}` early.
        let file = parse(&format!(
            "{LOOP}X-Last: {{{{ item | suffix(\"}}}}\") }}}}\n"
        ))
        .unwrap();
        let err = format!("{:#}", file.references(&file.requests[0]).unwrap_err());
        assert!(err.contains("missing \")\""), "{err}");

        let file = parse(&format!(
            "{LOOP}X-Last: {{{{ item | split(\":\", -1) | prefix(\"\") }}}}\n"
        ))
        .unwrap();
        let responses = HashMap::from([("login".to_string(), captured(BODY))]);
        let element = serde_json::json!({ "id": 7 });
        let r = file
            .resolve(&file.requests[0], &responses, Some(&element))
            .unwrap();
        assert_eq!(r.headers[1], ("X-Last".into(), "7}".into()));

        // SAFETY: tests that read this variable do not run concurrently.
        unsafe { std::env::set_var("AXON_FILTER_TEST", "  padded  ") };
        assert_eq!(
            filtered("", "$env AXON_FILTER_TEST | trim").unwrap(),
            "padded"
        );
    }

    #[test]
    fn invalid_filters_are_reported_when_checking() {
        for (expr, expected) in [
            (
                "v | splitt",
                "unknown filter \"splitt\" (expected split, trim, prefix or suffix)",
            ),
            (
                "v || trim",
                "unknown filter \"\" (expected split, trim, prefix or suffix)",
            ),
            ("v | split", "split takes a separator and an index"),
            (r#"v | split("/")"#, "split takes a separator and an index"),
            (
                r#"v | split("", 0)"#,
                "split takes a separator and an index",
            ),
            (
                r#"v | split(0, "/")"#,
                "split takes a separator and an index",
            ),
            (
                r#"v | split("/", 1.5)"#,
                "invalid arguments in split(\"/\", 1.5): expected a \"string\" or an integer: 1.5",
            ),
            (r#"v | split("/" 1)"#, "expected \",\" between arguments"),
            (
                r#"v | split("/", 1"#,
                "missing \")\" in filter split(\"/\", 1",
            ),
            (
                r#"v | prefix("a\n")"#,
                "only \\\" and \\\\ escapes are supported",
            ),
            (r#"v | prefix("a)"#, "unterminated string"),
            ("v | prefix", "prefix takes one string"),
            (r#"v | suffix("a", "b")"#, "suffix takes one string"),
            (r#"v | trim("a")"#, "trim takes no arguments"),
        ] {
            let err = format!(
                "{:#}",
                resolved(&format!("@v = x\nGET http://a/{{{{{expr}}}}}\n")).unwrap_err()
            );
            assert!(
                err.starts_with(&format!("invalid filter in {{{{{expr}}}}}")),
                "{expr}: {err}"
            );
            assert!(err.contains(expected), "{expr}: {err}");
        }
    }

    #[test]
    fn filter_errors_name_the_value() {
        for (expr, expected) in [
            (
                r#"login.response.headers.Location | split("/", 2)"#,
                r#"{{login.response.headers.Location | split("/", 2)}}: split("/", 2): "/next" gives 2 parts"#,
            ),
            (
                r#"login.response.headers.Location | split("/", -3)"#,
                r#"{{login.response.headers.Location | split("/", -3)}}: split("/", -3): "/next" gives 2 parts"#,
            ),
            (
                r#"login.response.cookies.session | prefix("xyz")"#,
                r#"{{login.response.cookies.session | prefix("xyz")}}: prefix("xyz"): "abc123" does not start with "xyz""#,
            ),
            (
                r#"login.response.cookies.session | suffix("abc")"#,
                r#"{{login.response.cookies.session | suffix("abc")}}: suffix("abc"): "abc123" does not end with "abc""#,
            ),
        ] {
            assert_eq!(format!("{:#}", filtered("", expr).unwrap_err()), expected);
        }
    }

    #[test]
    fn checking_does_not_apply_filters_to_unknown_values() {
        let file = parse(
            "@id = {{login.response.headers.Location | split(\"/\", 5)}}\nGET http://a/{{id | prefix(\"x\")}}/{{other.response.body.$ | split(\",\", 9)}}\n",
        )
        .unwrap();
        let names: Vec<_> = file
            .references(&file.requests[0])
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect();
        assert_eq!(names, ["login", "other"]);
    }

    #[test]
    fn splits_requests_on_separators() {
        let file = parse("### one\nGET http://a/1\n\n### two\nPOST http://a/2\n").unwrap();
        assert_eq!(
            file.requests,
            [
                request(Some("one"), "GET", "http://a/1", (1, 3)),
                request(Some("two"), "POST", "http://a/2", (4, 5))
            ]
        );
    }

    #[test]
    fn request_before_first_separator() {
        let file = parse("GET http://a/0\n### one\nGET http://a/1\n").unwrap();
        assert_eq!(file.requests[0], request(None, "GET", "http://a/0", (1, 1)));
        assert_eq!(file.requests[1].name.as_deref(), Some("one"));
    }

    #[test]
    fn line_ranges_cover_whole_blocks() {
        let file = parse(
            "@host = a\n\n### one\nPUT http://a\n\n{}\n# trailing comment\n\n### two\n// comment\nGET http://b\n### empty\n",
        )
        .unwrap();
        let ranges: Vec<_> = file
            .requests
            .iter()
            .map(|r| (r.start_line, r.end_line))
            .collect();
        assert_eq!(ranges, [(3, 8), (9, 11)]);
    }

    #[test]
    fn name_directive_overrides_separator_name() {
        let file = parse(
            "### Separator name\n# @name real\nGET http://a\n### x\n// @name other\nGET http://b\n",
        )
        .unwrap();
        assert_eq!(file.requests[0].name.as_deref(), Some("real"));
        assert_eq!(file.requests[1].name.as_deref(), Some("other"));
    }

    #[test]
    fn trailing_comment_is_not_part_of_name() {
        let file = parse(
            "### Set options // replaces all!\nGET http://a\n### x\n# @name real // note\nGET http://b\n### Fetch http://c\nGET http://c\n### // only a comment\nGET http://d\n",
        )
        .unwrap();
        let names: Vec<_> = file.requests.iter().map(|r| r.name.as_deref()).collect();
        assert_eq!(
            names,
            [
                Some("Set options"),
                Some("real"),
                Some("Fetch http://c"),
                None
            ]
        );
    }

    #[test]
    fn url_only_defaults_to_get() {
        let file = parse("http://a/x\n").unwrap();
        assert_eq!(file.requests[0], request(None, "GET", "http://a/x", (1, 1)));
    }

    #[test]
    fn method_is_uppercased_and_version_ignored() {
        let file = parse("patch http://a/x HTTP/1.1\n").unwrap();
        assert_eq!(
            file.requests[0],
            request(None, "PATCH", "http://a/x", (1, 1))
        );
    }

    #[test]
    fn headers_comments_and_body() {
        let file = parse(
            "# leading comment\nPOST http://a\nContent-Type: application/json\n// comment\nX-A: 1\n\n{\n  \"k\": 1\n}\n\n\n### next\nGET http://b\n",
        )
        .unwrap();
        let r = &file.requests[0];
        assert_eq!(
            r.headers,
            [
                ("Content-Type".into(), "application/json".into()),
                ("X-A".into(), "1".into())
            ]
        );
        assert_eq!(r.body.as_deref(), Some("{\n  \"k\": 1\n}"));
    }

    #[test]
    fn crlf_line_endings() {
        let file = parse("### a\r\nPOST http://a\r\nX-A: 1\r\n\r\nbody\r\n").unwrap();
        assert_eq!(file.requests[0].headers, [("X-A".into(), "1".into())]);
        assert_eq!(file.requests[0].body.as_deref(), Some("body"));
    }

    #[test]
    fn block_without_request_is_skipped() {
        let file =
            parse("@host = a\n### only a comment\n# nothing here\n### real\nGET http://{{host}}\n")
                .unwrap();
        assert_eq!(file.requests.len(), 1);
        assert_eq!(file.requests[0].name.as_deref(), Some("real"));
    }

    #[test]
    fn invalid_header_reports_line() {
        let err = parse("GET http://a\nno colon here\n").err().unwrap();
        assert_eq!(
            err.to_string(),
            "line 2: invalid header (expected \"Name: Value\"): no colon here"
        );
    }

    #[test]
    fn invalid_request_line() {
        let err = parse("GET http://a extra\n").err().unwrap();
        assert!(
            err.to_string().starts_with("line 1: invalid request line"),
            "{err}"
        );
    }

    #[test]
    fn substitutes_variables_everywhere() {
        let r = resolved("@host = example.com\n@token = t0k\nPOST https://{{host}}/x\nAuthorization: Bearer {{ token }}\n\nhost={{host}}\n").unwrap();
        assert_eq!(r.url, "https://example.com/x");
        assert_eq!(r.headers, [("Authorization".into(), "Bearer t0k".into())]);
        assert_eq!(r.body.as_deref(), Some("host=example.com"));
    }

    #[test]
    fn variables_reference_earlier_variables() {
        let r = resolved("@host = example.com\n@base = https://{{host}}/api\nGET {{base}}/items\n")
            .unwrap();
        assert_eq!(r.url, "https://example.com/api/items");
    }

    #[test]
    fn variable_cannot_reference_later_variable() {
        let err = resolved("@base = https://{{host}}\n@host = a\nGET {{base}}\n")
            .err()
            .unwrap();
        assert_eq!(err.to_string(), "undefined variable: host");
    }

    #[test]
    fn undefined_variable() {
        let err = resolved("GET http://{{nope}}\n").err().unwrap();
        assert_eq!(err.to_string(), "undefined variable: nope");
    }

    #[test]
    fn request_line_keeps_spaces_inside_braces() {
        let file = parse("GET http://a/{{$env HOST}}/x HTTP/1.1\n").unwrap();
        assert_eq!(file.requests[0].url, "http://a/{{$env HOST}}/x");
    }

    #[test]
    fn env_variable() {
        let path = std::env::var("PATH").unwrap();
        let r = resolved("GET http://a/?p={{$env PATH}}\n").unwrap();
        assert_eq!(r.url, format!("http://a/?p={path}"));
    }

    #[test]
    fn unset_env_variable() {
        let err = resolved("GET http://a/{{$env AXON_CLI_SURELY_UNSET}}\n")
            .err()
            .unwrap();
        assert_eq!(
            err.to_string(),
            "environment variable not set: AXON_CLI_SURELY_UNSET"
        );
    }

    #[test]
    fn unsupported_dynamic_variable() {
        let err = resolved("GET http://a/{{$uuid}}\n").err().unwrap();
        assert_eq!(err.to_string(), "unsupported dynamic variable: {{$uuid}}");
    }

    #[test]
    fn unterminated_braces() {
        let err = resolved("GET http://a/{{oops\n").err().unwrap();
        assert!(err.to_string().starts_with("unterminated \"{{\""), "{err}");
    }

    /// Runs the first request's checks against `own`, returning each
    /// failure as `LEVEL text: error` and passes as `ok text`.
    fn run_checks(contents: &str, own: &Captured, element: Option<&Value>) -> Vec<String> {
        let file = parse(contents).unwrap();
        let request = file.requests.last().unwrap();
        file.references(request).unwrap();
        let mut responses = HashMap::new();
        responses.insert("other".to_string(), captured(r#"{"id": 7}"#));
        file.check(request, own, &responses, element)
            .into_iter()
            .map(|r| match r.error {
                None => format!("ok {}", r.text),
                Some(error) => format!("{} {}: {error}", r.level.keyword(), r.text),
            })
            .collect()
    }

    #[test]
    fn parses_checks() {
        let file = parse(concat!(
            "### a\n",
            "# @assert status == 200\n",
            "// @warn {{response.headers.Content-Type | trim}} contains \"js\\\"on\"\n",
            "#   @notice   {{response.body.$.x}}   !exists  \n",
            "GET http://a/\n",
        ))
        .unwrap();
        let checks = &file.requests[0].checks;
        assert_eq!(checks.len(), 3);
        assert_eq!(
            (
                checks[0].level,
                checks[0].text.as_str(),
                checks[0].subject.as_str()
            ),
            (Level::Assert, "status == 200", "{{response.status}}")
        );
        assert_eq!(checks[0].expected.as_deref(), Some("200"));
        assert_eq!(
            (checks[1].level, checks[1].op, checks[1].expected.as_deref()),
            (Level::Warn, Op::Contains, Some("js\"on"))
        );
        assert_eq!(
            (checks[2].level, checks[2].op, checks[2].expected.as_deref()),
            (Level::Notice, Op::NotExists, None)
        );
        // `@assertion` is not a check, just a comment.
        let file = parse("# @assertion x\nGET http://a/\n").unwrap();
        assert!(file.requests[0].checks.is_empty());
    }

    #[test]
    fn invalid_checks_are_errors() {
        for (line, message) in [
            ("@assert", "expected status or {{...}} first"),
            ("@assert body == 1", "expected status or {{...}} first"),
            ("@assert status==200", "expected status or {{...}} first"),
            ("@assert {{x", "unterminated \"{{\""),
            ("@assert status", "missing operator"),
            ("@warn status =~ 2", "unknown operator \"=~\""),
            ("@notice status == ", "== needs a value"),
            ("@assert {{x}} exists 1", "exists takes no value"),
            ("@assert status == \"200", "unterminated string"),
            (
                "@assert status == \"2\" 0",
                "unexpected text after the closing quote: 0",
            ),
        ] {
            let err = parse(&format!("# {line}\nGET http://a/\n")).err().unwrap();
            let err = format!("{err:#}");
            assert!(err.starts_with("line 1: invalid @"), "{err}");
            assert!(err.contains(message), "{line}: {err}");
        }
        let err = parse("GET http://a/\n# @warn status == 200\n")
            .err()
            .unwrap();
        assert_eq!(
            err.to_string(),
            "line 2: @warn must come before the request line"
        );
    }

    #[test]
    fn checks_compare_values() {
        let mut own = captured(r#"{"id": 7, "name": "web", "count": 3, "tags": ["a"], "n": null}"#);
        own.status = 404;
        let results = run_checks(
            concat!(
                "@seven = 7\n",
                "# @assert status == 200\n",
                "# @assert status == 404\n",
                "# @assert {{response.status}} >= 400\n",
                "# @assert status < 400\n",
                "# @assert {{response.body.$.id}} == {{seven}}\n",
                "# @assert {{response.body.$.id}} == \"{{other.response.body.$.id}}\"\n",
                "# @assert {{response.body.$.name}} != \"web\"\n",
                "# @assert {{response.body.$.name}} contains \"e\"\n",
                "# @assert {{response.body.$.name}} !contains \"e\"\n",
                "# @assert {{response.body.$.tags}} == [\"a\"]\n",
                "# @assert {{response.body.$.n}} == null\n",
                "# @assert {{response.body.$.count}} > 2.5\n",
                "# @assert {{response.body.$.name}} > 1\n",
                "# @assert {{response.body.$.missing}} == 1\n",
                "# @assert {{response.body.$.missing}} exists\n",
                "# @assert {{response.body.$.missing}} !exists\n",
                "# @assert {{response.body.$.id}} !exists\n",
                "# @assert {{response.headers.location | split(\"/\", -1)}} == \"next\"\n",
                "# @assert {{response.headers.Location | split(\"/\", 5)}} exists\n",
                "# @assert {{response.cookies.session}} == \"abc123\"\n",
                "# @assert {{response.cookies.nope}} exists\n",
                "GET http://a/\n",
            ),
            &own,
            None,
        );
        assert_eq!(
            results,
            [
                "assert status == 200: got \"404\"",
                "ok status == 404",
                "ok {{response.status}} >= 400",
                "assert status < 400: got \"404\"",
                "ok {{response.body.$.id}} == {{seven}}",
                "ok {{response.body.$.id}} == \"{{other.response.body.$.id}}\"",
                "assert {{response.body.$.name}} != \"web\": got \"web\"",
                "ok {{response.body.$.name}} contains \"e\"",
                "assert {{response.body.$.name}} !contains \"e\": got \"web\"",
                "ok {{response.body.$.tags}} == [\"a\"]",
                "ok {{response.body.$.n}} == null",
                "ok {{response.body.$.count}} > 2.5",
                "assert {{response.body.$.name}} > 1: not a number: \"web\"",
                "assert {{response.body.$.missing}} == 1: response body has no value at $.missing",
                "assert {{response.body.$.missing}} exists: response body has no value at $.missing",
                "ok {{response.body.$.missing}} !exists",
                "assert {{response.body.$.id}} !exists: got \"7\"",
                "ok {{response.headers.location | split(\"/\", -1)}} == \"next\"",
                "assert {{response.headers.Location | split(\"/\", 5)}} exists: {{response.headers.Location | split(\"/\", 5)}}: split(\"/\", 5): \"/next\" gives 2 parts",
                "ok {{response.cookies.session}} == \"abc123\"",
                "assert {{response.cookies.nope}} exists: response sets no cookie nope",
            ]
        );
    }

    #[test]
    fn checks_see_the_loop_element() {
        let element = serde_json::json!({"id": 7});
        let results = run_checks(
            concat!(
                "### each\n",
                "# @foreach wc in {{other.response.body.$.list}}\n",
                "# @assert {{response.body.$.id}} == \"{{wc.id}}\"\n",
                "# @warn {{wc}} contains \"8\"\n",
                "GET http://a/{{wc.id}}\n",
            ),
            &captured(r#"{"id": 7}"#),
            Some(&element),
        );
        assert_eq!(
            results,
            [
                "ok {{response.body.$.id}} == \"{{wc.id}}\"",
                "warn {{wc}} contains \"8\": got \"{\\\"id\\\":7}\"",
            ]
        );
    }

    #[test]
    fn check_references_are_dependencies() {
        let file = parse(concat!(
            "@v = {{login.response.body.$.v}}\n",
            "### a\n",
            "# @assert {{response.body.$.v}} == \"{{v}}\"\n",
            "# @notice {{other.response.headers.X}} exists\n",
            "GET http://a/\n",
        ))
        .unwrap();
        let names: Vec<_> = file
            .references(&file.requests[0])
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect();
        assert_eq!(names, ["login", "other"]);

        for (check, message) in [
            (
                "{{nope}} exists",
                "@assert {{nope}} exists: undefined variable: nope",
            ),
            (
                "{{response.status.x}} exists",
                "@assert {{response.status.x}} exists: invalid response reference {{response.status.x}}",
            ),
            (
                "{{x | upper}} exists",
                "@assert {{x | upper}} exists: invalid filter in {{x | upper}}",
            ),
        ] {
            let file = parse(&format!("# @assert {check}\nGET http://a/\n")).unwrap();
            let err = format!("{:#}", file.references(&file.requests[0]).err().unwrap());
            assert!(err.starts_with(message), "{err}");
        }
        // `response.` only means the own response inside checks.
        let err = resolved("GET http://a/{{response.status}}\n")
            .err()
            .unwrap();
        assert_eq!(err.to_string(), "undefined variable: response.status");
        // A file variable cannot read the checked request's own response.
        let file =
            parse("@s = {{response.status}}\n# @assert {{s}} == 200\nGET http://a/\n").unwrap();
        let err = file.references(&file.requests[0]).err().unwrap();
        assert_eq!(
            format!("{err:#}"),
            "@assert {{s}} == 200: undefined variable: response.status"
        );
    }
}
