//! Parser for `.http` files (JetBrains / VS Code REST Client format):
//! `###` separators, `# @name`, `@var = value` file variables, and
//! `{{var}}` / `{{$env NAME}}` substitution, and response chaining with
//! `{{NAME.response.body.$.path}}` / `{{NAME.response.headers.Header}}` /
//! `{{NAME.response.cookies.Cookie}}`, filters such as
//! `{{value | split("/", -1) | trim}}`, loops with `# @foreach VAR in {{NAME.response.body.$.path}}`,
//! and response checks with `# @assert`, `# @warn` and `# @notice`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::Value;

#[derive(Default)]
pub struct HttpFile {
    /// File variables in definition order, with their values unsubstituted.
    variables: Vec<(String, String)>,
    pub requests: Vec<Request>,
}

/// A request as written in the file, or after [`HttpFile::resolve`].
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    pub name: Option<String>,
    /// The file the request is written in: the path given to
    /// [`parse_file`], or resolved from `import` lines; empty for [`parse`].
    pub file: String,
    /// Uppercased.
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
    /// Set by `# @foreach VAR in {{...}}` lines: the request runs once per
    /// element, nested in the order written.
    pub foreach: Vec<Foreach>,
    /// `# @assert`, `# @warn` and `# @notice` checks, in the order written.
    pub checks: Vec<Check>,
    /// `# @skip-if` conditions: an iteration is not sent when one is true.
    pub skips: Vec<Check>,
    /// `# @let NAME = VALUE` request-local variables, in the order written.
    pub lets: Vec<Let>,
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

    /// The keyword of the directive that reports when its check is true.
    pub fn if_keyword(self) -> &'static str {
        match self {
            Level::Assert => "fail-if",
            Level::Warn => "warn-if",
            Level::Notice => "notice-if",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Op {
    Eq,
    Ne,
    In,
    NotIn,
    Contains,
    NotContains,
    Lt,
    Le,
    Gt,
    Ge,
    Exists,
    NotExists,
}

/// A `# @assert SUBJECT OPERATOR [EXPECTED]` line (or `@warn`, `@notice`,
/// and `@fail-if`, `@warn-if`, `@notice-if`).
#[derive(Clone, Debug, PartialEq)]
pub struct Check {
    pub level: Level,
    /// Written as `@fail-if`, `@warn-if` or `@notice-if`: reported when the
    /// comparison is true rather than false.
    pub inverted: bool,
    /// The directive keyword, without `@`: `assert`, `warn-if`, `skip-if`…
    keyword: &'static str,
    /// The check as written after the keyword, for messages.
    pub text: String,
    /// One `{{...}}`, as written; `status` is `{{response.status}}`.
    subject: String,
    op: Op,
    /// Texts to substitute and compare with, their quotes removed: none for
    /// `exists` and `!exists`, one or more for `in` and `!in`, else one.
    expected: Vec<String>,
}

/// The outcome of one check against one response.
#[derive(Clone, Debug, PartialEq)]
pub struct CheckResult {
    pub level: Level,
    /// From a `-if` directive.
    pub inverted: bool,
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

/// A `# @let NAME = VALUE` request-local variable.
#[derive(Clone, Debug, PartialEq)]
pub struct Let {
    pub name: String,
    /// Substituted for each iteration.
    pub value: String,
}

/// A `# @foreach VAR in {{SOURCE}}` directive.
#[derive(Clone, Debug, PartialEq)]
pub struct Foreach {
    /// The loop variable, used as `{{VAR}}`, `{{VAR.key}}`, `{{VAR[N]}}`.
    pub var: String,
    /// The array to loop over.
    source: Source,
}

/// What a `@foreach` loops over.
#[derive(Clone, Debug, PartialEq)]
enum Source {
    /// A response body reference, `{{NAME.response.body.$.path}}`.
    Response(Reference),
    /// A value inside an earlier loop variable, `{{VAR.path}}`.
    Var {
        /// As written, for messages.
        text: String,
        var: String,
        steps: Vec<Step>,
    },
}

impl Source {
    /// The source as written, for messages.
    fn text(&self) -> String {
        match self {
            Source::Response(reference) => reference.text(),
            Source::Var { text, .. } => text.clone(),
        }
    }
}

/// A loop variable's element in one iteration, with its 0-based index.
#[derive(Clone, Debug, PartialEq)]
pub struct Binding {
    pub var: String,
    pub index: usize,
    pub value: Value,
}

/// What one iteration of a request (or the one run of a request that does
/// not loop) can read.
#[derive(Clone, Copy)]
pub struct Iteration<'a> {
    /// Responses by request name.
    pub responses: &'a HashMap<String, Captured>,
    /// The looping request being followed, with its response in this
    /// iteration; it wins over `responses`.
    pub followed: Option<(&'a str, &'a Captured)>,
    /// Loop variables, outermost first.
    pub vars: &'a [Binding],
}

impl<'a> Iteration<'a> {
    fn response(&self, name: &str) -> Result<&'a Captured> {
        if let Some((followed, response)) = self.followed
            && followed == name
        {
            return Ok(response);
        }
        self.responses
            .get(name)
            .with_context(|| format!("no response from \"{name}\" yet"))
    }

    fn items(&self) -> Vec<Item<'a>> {
        self.vars
            .iter()
            .map(|binding| Item {
                var: &binding.var,
                element: Some(&binding.value),
            })
            .collect()
    }
}

/// Why expanding a request's `@foreach` lines failed, with the loop
/// variables of the iteration being expanded.
#[derive(Debug)]
pub struct IterationError {
    pub vars: Vec<Binding>,
    pub error: anyhow::Error,
}

/// A loop variable in scope while substituting, with its current element
/// (`None` when only checking).
#[derive(Clone, Copy)]
struct Item<'a> {
    var: &'a str,
    element: Option<&'a Value>,
}

/// What a `{{...}}` can see besides file variables, `$env` and other
/// requests' responses.
#[derive(Clone, Copy, Default)]
struct Scope<'a> {
    /// Loop variables, outermost first.
    items: &'a [Item<'a>],
    /// In a check, the request's own response, read by `{{response...}}`
    /// (`Some(None)` when only checking).
    own: Option<Option<&'a Captured>>,
    /// While collecting references before the loop variables in scope are
    /// known, an undefined variable is unknown rather than an error.
    lenient: bool,
    /// The request's `@let` variables evaluated so far (`None` values when
    /// only checking).
    lets: &'a [(String, Option<String>)],
}

/// Letters, digits, `_` and `-`.
fn is_var_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
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

/// Parses the text after `@foreach`: `VAR in {{NAME.response.body.$.path}}`
/// or `VAR in {{VAR.path}}`.
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
    if !is_var_name(var) {
        bail!("invalid @foreach variable (use letters, digits, _ and -): {var}");
    }
    if split_pipes(source).len() > 1 {
        bail!("@foreach cannot use filters: {{{{{source}}}}}");
    }
    let source = match Reference::parse(source) {
        Some(reference) => {
            let reference = reference?;
            let kind = match reference.part {
                Part::Body { .. } => None,
                Part::Header(_) => Some("header"),
                Part::Cookie(_) => Some("cookie"),
                Part::Status => Some("status"),
            };
            if let Some(kind) = kind {
                bail!(
                    "@foreach must loop over a response body, not a {kind}: {{{{{}}}}}",
                    reference.text()
                );
            }
            Source::Response(reference)
        }
        None => {
            let end = source.find(['.', '[']).unwrap_or(source.len());
            let source_var = &source[..end];
            if !is_var_name(source_var) {
                bail!(
                    "@foreach must loop over a {{{{NAME.response.body.$.path}}}} reference or a loop variable: {{{{{source}}}}}"
                );
            }
            let steps = parse_path(&format!("${}", &source[end..]))
                .with_context(|| format!("invalid loop variable reference {{{{{source}}}}}"))?;
            Source::Var {
                text: source.to_string(),
                var: source_var.to_string(),
                steps,
            }
        }
    };
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

/// Parses the text after `@assert`, `@warn`, `@notice` or their `-if` forms:
/// `SUBJECT OPERATOR [EXPECTED]`.
fn parse_check(keyword: &'static str, level: Level, inverted: bool, text: &str) -> Result<Check> {
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
        "in" => Op::In,
        "!in" => Op::NotIn,
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
                "unknown operator \"{other}\"; expected ==, !=, in, !in, contains, !contains, <, <=, >, >=, exists or !exists"
            )));
        }
    };
    let expected = match (
        matches!(op, Op::Exists | Op::NotExists),
        expected.is_empty(),
    ) {
        (true, true) => Vec::new(),
        (true, false) => return Err(invalid(format!("{op_text} takes no value"))),
        (false, true) => return Err(invalid(format!("{op_text} needs a value"))),
        (false, false) if matches!(op, Op::In | Op::NotIn) => {
            let mut values = Vec::new();
            for item in split_list(expected) {
                if item.is_empty() {
                    return Err(invalid(format!("{op_text} has an empty value")));
                }
                values.push(expected_value(item).map_err(|e| invalid(e.to_string()))?);
            }
            values
        }
        (false, false) => vec![expected_value(expected).map_err(|e| invalid(e.to_string()))?],
    };
    Ok(Check {
        level,
        inverted,
        keyword,
        text: text.to_string(),
        subject,
        op,
        expected,
    })
}

/// An EXPECTED value: a double-quoted string, its quotes and escapes
/// removed, or bare text.
fn expected_value(text: &str) -> Result<String> {
    let Some(after) = text.strip_prefix('"') else {
        return Ok(text.to_string());
    };
    let (value, rest) = parse_quoted(after)?;
    if !rest.trim().is_empty() {
        bail!("unexpected text after the closing quote: {}", rest.trim());
    }
    Ok(value)
}

/// Splits an `in` list on commas outside double quotes and `{{...}}`,
/// trimming each item.
fn split_list(text: &str) -> Vec<&str> {
    let mut items = Vec::new();
    let mut start = 0;
    let mut depth = 0;
    let mut quoted = false;
    let mut escaped = false;
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if quoted {
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                quoted = false;
            }
        } else if c == b'"' {
            quoted = true;
        } else if text[i..].starts_with("{{") {
            depth += 1;
            i += 1;
        } else if depth > 0 && text[i..].starts_with("}}") {
            depth -= 1;
            i += 1;
        } else if c == b',' && depth == 0 {
            items.push(text[start..i].trim());
            start = i + 1;
        }
        i += 1;
    }
    items.push(text[start..].trim());
    items
}

/// `assert`, or `fail-if` for an inverted check.
fn directive_keyword(level: Level, inverted: bool) -> &'static str {
    if inverted {
        level.if_keyword()
    } else {
        level.keyword()
    }
}

impl Check {
    /// The directive as written, with `@`.
    pub fn directive(&self) -> String {
        format!("@{}", self.keyword)
    }

    /// Whether the comparison is true; an unreadable value is an error,
    /// except for `exists` and `!exists`.
    fn holds(&self, eval: &mut dyn FnMut(&str) -> Result<String>) -> Result<bool> {
        let subject = eval(&self.subject);
        match self.op {
            Op::Exists => Ok(subject.is_ok()),
            Op::NotExists => Ok(subject.is_err()),
            _ => {
                let actual = subject?;
                self.compare(&actual, eval)
            }
        }
    }

    /// The `{{...}}` texts it substitutes.
    fn texts(&self) -> impl Iterator<Item = &String> {
        std::iter::once(&self.subject).chain(&self.expected)
    }

    /// Compares the subject with the expected value; `eval` substitutes a
    /// `{{...}}` text. The error says why the check failed: the comparison
    /// came out false (true for an inverted check), or a value could not be
    /// read, which fails either way except for `exists` and `!exists`.
    fn evaluate(&self, eval: &mut dyn FnMut(&str) -> Result<String>) -> Result<()> {
        let subject = eval(&self.subject);
        let (holds, actual) = match self.op {
            Op::Exists => (subject.is_ok(), subject),
            Op::NotExists => (subject.is_err(), subject),
            _ => {
                let actual = subject?;
                (self.compare(&actual, eval)?, Ok(actual))
            }
        };
        if holds != self.inverted {
            return Ok(());
        }
        let word = if self.inverted { "matched" } else { "got" };
        // A missing value is its own explanation.
        let value = actual?;
        Err(anyhow::anyhow!("{word} {value:?}"))
    }

    /// Whether the comparison of `actual` with the expected value is true.
    fn compare(&self, actual: &str, eval: &mut dyn FnMut(&str) -> Result<String>) -> Result<bool> {
        if matches!(self.op, Op::In | Op::NotIn) {
            let mut found = false;
            for expected in &self.expected {
                found |= eval(expected)? == actual;
            }
            return Ok(found == (self.op == Op::In));
        }
        let expected = eval(self.expected.first().map_or("", String::as_str))?;
        Ok(match self.op {
            Op::Eq => actual == expected,
            Op::Ne => actual != expected,
            Op::Contains => actual.contains(&expected),
            Op::NotContains => !actual.contains(&expected),
            Op::In | Op::NotIn | Op::Exists | Op::NotExists => unreachable!("handled above"),
            op => {
                let (a, e) = (number(actual)?, number(&expected)?);
                match op {
                    Op::Lt => a < e,
                    Op::Le => a <= e,
                    Op::Gt => a > e,
                    _ => a >= e,
                }
            }
        })
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

/// Parses `.http` text that is not read from a file; `import` paths are
/// relative to the current directory.
pub fn parse(contents: &str) -> Result<HttpFile> {
    let mut file = HttpFile::default();
    file.parse_text(contents, "", &mut Imports::default())
        .map_err(unnest)?;
    Ok(file)
}

/// Reads and parses a `.http` file, including the files it imports.
pub fn parse_file(path: &Path) -> Result<HttpFile> {
    let display = path.display().to_string();
    let contents =
        std::fs::read_to_string(path).with_context(|| format!("failed to read {display}"))?;
    let mut imports = Imports::default();
    if let Ok(canonical) = std::fs::canonicalize(path) {
        imports.included.insert(canonical.clone());
        imports.stack.push((canonical, display.clone()));
    }
    let mut file = HttpFile::default();
    file.parse_text(&contents, &display, &mut imports)
        .map_err(unnest)?;
    Ok(file)
}

/// The files included so far in one parse, by canonical path, and the chain
/// of files being imported (with their displayed paths), to detect cycles.
#[derive(Default)]
struct Imports {
    included: HashSet<PathBuf>,
    stack: Vec<(PathBuf, String)>,
}

/// The file whose lines are being parsed.
struct Origin<'a> {
    /// As shown in messages and the listing; empty for [`parse`].
    display: &'a str,
    /// A request has been parsed, so `import` lines are no longer allowed.
    requests_seen: bool,
}

/// An error from an imported file, which already names that file, passed up
/// through the importing files unchanged.
#[derive(Debug)]
struct Nested(anyhow::Error);

impl std::fmt::Display for Nested {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.0)
    }
}

impl std::error::Error for Nested {}

/// The error inside a [`Nested`] one, or the error itself.
fn unnest(err: anyhow::Error) -> anyhow::Error {
    match err.downcast::<Nested>() {
        Ok(Nested(inner)) => inner,
        Err(err) => err,
    }
}

/// The path of an `import PATH` line, an error for `import` without one, or
/// `None` when the line is not an import.
fn import_line(line: &str) -> Option<Result<&str>> {
    let rest = line.strip_prefix("import")?;
    if rest.trim().is_empty() {
        return Some(Err(anyhow::anyhow!("import needs a path")));
    }
    rest.starts_with(char::is_whitespace)
        .then(|| Ok(rest.trim()))
}

impl HttpFile {
    /// Parses the lines of one file, `display` naming it in errors.
    fn parse_text(&mut self, contents: &str, display: &str, imports: &mut Imports) -> Result<()> {
        let mut origin = Origin {
            display,
            requests_seen: false,
        };
        self.parse_lines(contents, &mut origin, imports)
            .map_err(|err| {
                if err.is::<Nested>() || display.is_empty() {
                    err
                } else {
                    err.context(format!("failed to parse {display}"))
                }
            })
    }

    fn parse_lines(
        &mut self,
        contents: &str,
        origin: &mut Origin,
        imports: &mut Imports,
    ) -> Result<()> {
        let mut block = Vec::new();
        let mut name = None;
        let mut start = 1;
        let mut last = 0;
        for (index, line) in contents.lines().enumerate() {
            last = index + 1;
            if let Some(rest) = line.strip_prefix("###") {
                self.parse_block(name.take(), (start, last - 1), &block, origin, imports)?;
                block.clear();
                name = clean_name(rest);
                start = last;
            } else {
                block.push((last, line));
            }
        }
        self.parse_block(name, (start, last), &block, origin, imports)
    }

    /// Adds the variables and requests of the file at `path`, relative to
    /// the importing file, unless it is already included.
    fn import(
        &mut self,
        path: &str,
        line_no: usize,
        origin: &Origin,
        imports: &mut Imports,
    ) -> Result<()> {
        let target = Path::new(origin.display)
            .parent()
            .unwrap_or(Path::new(""))
            .join(path);
        let display = target.display().to_string();
        let importer = if origin.display.is_empty() {
            "input"
        } else {
            origin.display
        };
        let unreadable = |err: std::io::Error| {
            Nested(anyhow::Error::new(err).context(format!(
                "failed to read {display} (imported by {importer} line {line_no})"
            )))
        };
        let canonical = std::fs::canonicalize(&target).map_err(unreadable)?;
        if let Some(start) = imports.stack.iter().position(|(c, _)| *c == canonical) {
            let chain: Vec<_> = imports.stack[start..]
                .iter()
                .map(|(_, shown)| shown.as_str())
                .chain([display.as_str()])
                .collect();
            bail!("line {line_no}: import cycle: {}", chain.join(" -> "));
        }
        if !imports.included.insert(canonical.clone()) {
            return Ok(());
        }
        let contents = std::fs::read_to_string(&target).map_err(unreadable)?;
        imports.stack.push((canonical, display.clone()));
        let result = self.parse_text(&contents, &display, imports);
        imports.stack.pop();
        result.map_err(|err| {
            if err.is::<Nested>() {
                err
            } else {
                Nested(err).into()
            }
        })
    }

    /// Parses the lines between two `###` separators. A block without a
    /// request line (e.g. only variables or comments) adds no request.
    fn parse_block(
        &mut self,
        mut name: Option<String>,
        (start_line, end_line): (usize, usize),
        lines: &[(usize, &str)],
        origin: &mut Origin,
        imports: &mut Imports,
    ) -> Result<()> {
        let mut lines = lines.iter();
        let mut foreach: Vec<Foreach> = Vec::new();
        let mut checks = Vec::new();
        let mut lets = Vec::new();
        let mut skips = Vec::new();

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
                if foreach.iter().any(|f| f.var == directive.var) {
                    bail!(
                        "line {line_no}: @foreach variable \"{}\" is used twice",
                        directive.var
                    );
                }
                foreach.push(directive);
            } else if let Some((_, check)) = check_directive(line) {
                checks.push(check.with_context(|| format!("line {line_no}"))?);
            } else if let Some(skip) = skip_directive(line) {
                skips.push(skip.with_context(|| format!("line {line_no}"))?);
            } else if let Some(directive) = let_directive(line) {
                let directive = directive.with_context(|| format!("line {line_no}"))?;
                if lets.iter().any(|l: &Let| l.name == directive.name) {
                    bail!(
                        "line {line_no}: @let variable \"{}\" is defined twice",
                        directive.name
                    );
                }
                lets.push(directive);
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
            } else if let Some(path) = import_line(line) {
                if origin.requests_seen {
                    bail!("line {line_no}: import must come before the first request");
                }
                let path = path.with_context(|| format!("line {line_no}"))?;
                self.import(path, line_no, origin, imports)?;
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
            } else if import_line(line).is_some() {
                bail!("line {line_no}: import must come before the first request");
            } else if let_directive(line).is_some() {
                bail!("line {line_no}: @let must come before the request line");
            } else if skip_directive(line).is_some() {
                bail!("line {line_no}: @skip-if must come before the request line");
            } else if let Some((keyword, _)) = check_directive(line) {
                bail!(
                    "line {line_no}: @{} must come before the request line",
                    keyword
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

        origin.requests_seen = true;
        self.requests.push(Request {
            name,
            file: origin.display.to_string(),
            method: method.to_ascii_uppercase(),
            url: url.to_string(),
            headers,
            body: (!body.is_empty()).then(|| body.join("\n")),
            foreach,
            checks,
            skips,
            lets,
            start_line,
            end_line,
        });
        Ok(())
    }

    /// Substitutes variables in the request's URL, headers and body for one
    /// iteration (or the one run of a request that does not loop).
    pub fn resolve(&self, request: &Request, ctx: &Iteration) -> Result<Request> {
        let items = ctx.items();
        let scope = Scope {
            items: &items,
            ..Scope::default()
        };
        let mut on_reference = |reference: &Reference| -> Result<Option<String>> {
            reference.extract(ctx.response(&reference.name)?).map(Some)
        };
        let lets = self.let_values(request, scope, &mut on_reference)?;
        self.substitute_request(
            request,
            Scope {
                lets: &lets,
                ..scope
            },
            &mut on_reference,
        )
    }

    /// Whether one of the request's `@skip-if` conditions is true in this
    /// iteration, so it is not sent.
    pub fn skip(&self, request: &Request, ctx: &Iteration) -> Result<bool> {
        if request.skips.is_empty() {
            return Ok(false);
        }
        let items = ctx.items();
        let scope = Scope {
            items: &items,
            ..Scope::default()
        };
        let mut on_reference = |reference: &Reference| -> Result<Option<String>> {
            reference.extract(ctx.response(&reference.name)?).map(Some)
        };
        let lets = self.let_values(request, scope, &mut on_reference)?;
        let scope = Scope {
            lets: &lets,
            ..scope
        };
        for skip in &request.skips {
            let mut eval = |text: &str| {
                self.substitute(text, None, scope, &mut on_reference)
                    .map(Option::unwrap_or_default)
            };
            if skip
                .holds(&mut eval)
                .with_context(|| format!("{} {}", skip.directive(), skip.text))?
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The request's `@let` variables, in order, each seeing the earlier ones.
    fn let_values(
        &self,
        request: &Request,
        scope: Scope,
        on_reference: OnReference,
    ) -> Result<Vec<(String, Option<String>)>> {
        let mut values = Vec::new();
        for each in &request.lets {
            let value = self
                .substitute(
                    &each.value,
                    None,
                    Scope {
                        lets: &values,
                        ..scope
                    },
                    on_reference,
                )
                .with_context(|| format!("@let {}", each.name))?;
            values.push((each.name.clone(), value));
        }
        Ok(values)
    }

    /// The names of the requests this request references, in order of
    /// appearance (`@foreach` sources first). Unlike [`HttpFile::references`]
    /// it does not need to know the loop variables in scope, which may come
    /// from a looping request it follows.
    pub fn referenced_names(&self, request: &Request) -> Result<Vec<String>> {
        let mut names: Vec<String> = Vec::new();
        for reference in self.collect_references(request, &[], true)? {
            if !names.contains(&reference.name) {
                names.push(reference.name);
            }
        }
        Ok(names)
    }

    /// The distinct response references in the request, including those
    /// reached through file variables and `@foreach` sources (first), in
    /// order of appearance. Also checks the request's variables, like
    /// [`HttpFile::resolve`]; `inherited` are the loop variables of the
    /// looping request it follows, if any.
    pub fn references(&self, request: &Request, inherited: &[String]) -> Result<Vec<Reference>> {
        let mut known: Vec<&str> = inherited.iter().map(String::as_str).collect();
        for foreach in &request.foreach {
            let var = foreach.var.as_str();
            if self.variables.iter().any(|(name, _)| name == var) {
                bail!("@foreach variable \"{var}\" has the same name as a file variable");
            }
            if known.contains(&var) {
                bail!("@foreach variable \"{var}\" is already a loop variable");
            }
            if let Source::Var {
                text,
                var: source_var,
                ..
            } = &foreach.source
                && !known.contains(&source_var.as_str())
            {
                bail!(
                    "@foreach must loop over a {{{{NAME.response.body.$.path}}}} reference or an earlier loop variable: {{{{{text}}}}}"
                );
            }
            known.push(var);
        }
        for skip in &request.skips {
            if request.foreach.is_empty() && inherited.is_empty() {
                bail!("@skip-if is only for a looping request (@foreach)");
            }
            if skip.texts().any(|text| uses_own_response(text)) {
                bail!(
                    "@skip-if {}: cannot read {{{{response...}}}}, since nothing has been sent yet",
                    skip.text
                );
            }
        }
        for each in &request.lets {
            let name = each.name.as_str();
            if self.variables.iter().any(|(var, _)| var == name) {
                bail!("@let variable \"{name}\" has the same name as a file variable");
            }
            if known.contains(&name) {
                bail!("@let variable \"{name}\" has the same name as a loop variable");
            }
        }
        self.collect_references(request, &known, false)
    }

    /// The distinct response references in the request, with `vars` the
    /// loop variables in scope; `lenient` leaves undefined variables unknown.
    fn collect_references(
        &self,
        request: &Request,
        vars: &[&str],
        lenient: bool,
    ) -> Result<Vec<Reference>> {
        let mut found: Vec<Reference> = Vec::new();
        for foreach in &request.foreach {
            if let Source::Response(reference) = &foreach.source
                && !found.contains(reference)
            {
                found.push(reference.clone());
            }
        }
        let items: Vec<Item> = vars
            .iter()
            .map(|&var| Item { var, element: None })
            .collect();
        let scope = Scope {
            items: &items,
            lenient,
            ..Scope::default()
        };
        let mut collect = |reference: &Reference| -> Result<Option<String>> {
            if !found.contains(reference) {
                found.push(reference.clone());
            }
            Ok(None)
        };
        let lets = self.let_values(request, scope, &mut collect)?;
        let scope = Scope {
            lets: &lets,
            ..scope
        };
        self.substitute_request(request, scope, &mut collect)?;
        // A check's `{{response...}}` is its own response, not a dependency.
        let scope = Scope {
            own: Some(None),
            ..scope
        };
        for check in &request.checks {
            for text in check.texts() {
                self.substitute(text, None, scope, &mut collect)
                    .with_context(|| format!("{} {}", check.directive(), check.text))?;
            }
        }
        // Nothing has been sent when a @skip-if is evaluated.
        let scope = Scope { own: None, ..scope };
        for skip in &request.skips {
            for text in skip.texts() {
                self.substitute(text, None, scope, &mut collect)
                    .with_context(|| format!("{} {}", skip.directive(), skip.text))?;
            }
        }
        Ok(found)
    }

    /// The loop variables of each iteration of the request's own `@foreach`
    /// lines, nested in the order written, to add after those in `ctx`; one
    /// empty list for a request without `@foreach`.
    pub fn iterations(
        &self,
        request: &Request,
        ctx: &Iteration,
    ) -> std::result::Result<Vec<Vec<Binding>>, IterationError> {
        let mut combinations = vec![Vec::new()];
        for foreach in &request.foreach {
            let mut next = Vec::new();
            for vars in combinations {
                let items = match self.source_value(&foreach.source, ctx, &vars) {
                    Ok(Value::Array(items)) => items,
                    Ok(_) => {
                        let error = anyhow::anyhow!(
                            "@foreach: {{{{{}}}}} is not an array",
                            foreach.source.text()
                        );
                        return Err(IterationError { vars, error });
                    }
                    Err(error) => return Err(IterationError { vars, error }),
                };
                for (index, value) in items.into_iter().enumerate() {
                    let mut inner = vars.clone();
                    inner.push(Binding {
                        var: foreach.var.clone(),
                        index,
                        value,
                    });
                    next.push(inner);
                }
            }
            combinations = next;
        }
        Ok(combinations)
    }

    /// The value a `@foreach` loops over, with `own` the request's own loop
    /// variables bound so far.
    fn source_value(&self, source: &Source, ctx: &Iteration, own: &[Binding]) -> Result<Value> {
        match source {
            Source::Response(reference) => reference.value(ctx.response(&reference.name)?),
            Source::Var { text, var, steps } => {
                let binding = own
                    .iter()
                    .rev()
                    .chain(ctx.vars.iter().rev())
                    .find(|binding| binding.var == *var)
                    .with_context(|| format!("undefined loop variable: {var}"))?;
                walk(&binding.value, steps)
                    .cloned()
                    .with_context(|| format!("current element has no value at {{{{{text}}}}}"))
            }
        }
    }

    /// Evaluates the request's checks against `response`, its own response,
    /// in the iteration described by `ctx`.
    pub fn check(
        &self,
        request: &Request,
        response: &Captured,
        ctx: &Iteration,
    ) -> Vec<CheckResult> {
        let items = ctx.items();
        let mut on_reference = |reference: &Reference| -> Result<Option<String>> {
            reference.extract(ctx.response(&reference.name)?).map(Some)
        };
        let scope = Scope {
            items: &items,
            ..Scope::default()
        };
        // `@let` values do not see the response, so they are as when sent.
        let lets = self.let_values(request, scope, &mut on_reference);
        let (lets, lets_error) = match lets {
            Ok(lets) => (lets, None),
            Err(err) => (Vec::new(), Some(format!("{err:#}"))),
        };
        let scope = Scope {
            own: Some(Some(response)),
            lets: &lets,
            ..scope
        };
        request
            .checks
            .iter()
            .map(|check| {
                let mut eval = |text: &str| {
                    self.substitute(text, None, scope, &mut on_reference)
                        .map(Option::unwrap_or_default)
                };
                CheckResult {
                    level: check.level,
                    inverted: check.inverted,
                    text: check.text.clone(),
                    error: match &lets_error {
                        Some(error) => Some(error.clone()),
                        None => check.evaluate(&mut eval).err().map(|e| format!("{e:#}")),
                    },
                }
            })
            .collect()
    }

    fn substitute_request(
        &self,
        request: &Request,
        scope: Scope,
        on_reference: OnReference,
    ) -> Result<Request> {
        // While only checking, unknown values are left empty.
        let mut substitute = |text| {
            Ok::<_, anyhow::Error>(
                self.substitute(text, None, scope, on_reference)?
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
            file: request.file.clone(),
            method: request.method.clone(),
            url,
            headers,
            body,
            foreach: request.foreach.clone(),
            checks: request.checks.clone(),
            skips: request.skips.clone(),
            lets: request.lets.clone(),
            start_line: request.start_line,
            end_line: request.end_line,
        })
    }

    /// Replaces each `{{...}}` in `text`, with file variables, what `scope`
    /// adds and response references; `chain` holds the file variables being
    /// substituted, if any. `None` when a value is not known because only
    /// the request is being checked.
    fn substitute(
        &self,
        text: &str,
        chain: Option<&Resolving>,
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
            match self.lookup(after[..end].trim(), chain, scope, on_reference)? {
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
        chain: Option<&Resolving>,
        scope: Scope,
        on_reference: OnReference,
    ) -> Result<Option<String>> {
        let mut parts = split_pipes(text).into_iter();
        let name = parts.next().unwrap_or_default();
        let filters = parts
            .map(Filter::parse)
            .collect::<Result<Vec<_>>>()
            .with_context(|| format!("invalid filter in {{{{{text}}}}}"))?;
        let Some(mut value) = self.lookup_value(name, chain, scope, on_reference)? else {
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
        chain: Option<&Resolving>,
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
            .items
            .iter()
            .rev()
            .find_map(|item| Some((item, loop_path(name, item.var)?)))
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
        if let Some((_, value)) = scope.lets.iter().rev().find(|(n, _)| n == name) {
            return Ok(value.clone());
        }
        if let Some(reference) = Reference::parse(name) {
            return on_reference(&reference?);
        }
        // A redefined variable takes its last definition (late binding),
        // except inside its own value, where it takes the one before.
        let definitions = match chain.and_then(|chain| chain.index_of(name)) {
            Some(own) => &self.variables[..own],
            None => &self.variables[..],
        };
        let Some(index) = definitions.iter().rposition(|(n, _)| n == name) else {
            if scope.lenient {
                return Ok(None);
            }
            bail!("undefined variable: {name}");
        };
        // The loop variable and a check's own response are not visible
        // inside file variables.
        let link = Resolving {
            name,
            index,
            parent: chain,
        };
        self.substitute(
            &self.variables[index].1,
            Some(&link),
            Scope::default(),
            on_reference,
        )
    }
}

/// The file variables being substituted, innermost first: a variable used
/// inside its own value takes its definition before this one, which also
/// ends any cycle between variables.
struct Resolving<'a> {
    name: &'a str,
    /// The definition being substituted, in [`HttpFile`]'s variables.
    index: usize,
    parent: Option<&'a Resolving<'a>>,
}

impl Resolving<'_> {
    /// The definition of `name` being substituted, innermost first.
    fn index_of(&self, name: &str) -> Option<usize> {
        let mut link = Some(self);
        while let Some(current) = link {
            if current.name == name {
                return Some(current.index);
            }
            link = current.parent;
        }
        None
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

/// The directive keyword and parsed check of a `# @assert ...`,
/// `# @warn ...`, `# @notice ...` line or one of their `-if` forms (or with
/// `//`), or `None` when the line is not a check.
fn check_directive(line: &str) -> Option<(&'static str, Result<Check>)> {
    let rest = line.strip_prefix('#').or_else(|| line.strip_prefix("//"))?;
    let rest = rest.trim_start().strip_prefix('@')?;
    Level::ALL
        .into_iter()
        .flat_map(|level| [(level, false), (level, true)])
        .find_map(|(level, inverted)| {
            let keyword = directive_keyword(level, inverted);
            let text = rest.strip_prefix(keyword)?;
            if !text.is_empty() && !text.starts_with(char::is_whitespace) {
                return None;
            }
            Some((keyword, parse_check(keyword, level, inverted, text.trim())))
        })
}

/// The parsed `# @skip-if CHECK` or `// @skip-if ...` line, or `None` when
/// the line is not a `@skip-if` directive.
fn skip_directive(line: &str) -> Option<Result<Check>> {
    let rest = line.strip_prefix('#').or_else(|| line.strip_prefix("//"))?;
    let rest = rest.trim_start().strip_prefix("@skip-if")?;
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    Some(parse_check("skip-if", Level::Notice, true, rest.trim()))
}

/// Whether a `{{...}}` in `text` reads `response...`, the checked request's
/// own response.
fn uses_own_response(text: &str) -> bool {
    text.split("{{")
        .skip(1)
        .any(|part| part.trim_start().starts_with("response."))
}

/// The parsed `# @let NAME = VALUE` or `// @let ...` line, or `None` when
/// the line is not a `@let` directive.
fn let_directive(line: &str) -> Option<Result<Let>> {
    let rest = line.strip_prefix('#').or_else(|| line.strip_prefix("//"))?;
    let rest = rest.trim_start().strip_prefix("@let")?;
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let text = rest.trim();
    Some(match text.split_once('=') {
        Some((name, value)) if is_var_name(name.trim()) => Ok(Let {
            name: name.trim().to_string(),
            value: value.trim().to_string(),
        }),
        Some((name, _)) => Err(anyhow::anyhow!(
            "invalid @let variable (use letters, digits, _ and -): {}",
            name.trim()
        )),
        None => Err(anyhow::anyhow!(
            "invalid @let (expected \"@let NAME = VALUE\"): @let {text}"
        )),
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
            file: String::new(),
            method: method.into(),
            url: url.into(),
            headers: Vec::new(),
            body: None,
            foreach: Vec::new(),
            checks: Vec::new(),
            skips: Vec::new(),
            lets: Vec::new(),
            start_line,
            end_line,
        }
    }

    fn resolved(contents: &str) -> Result<Request> {
        let file = parse(contents).unwrap();
        file.resolve(&file.requests[0], &at(&HashMap::new(), &[]))
    }

    /// An iteration with these responses and loop variables.
    fn at<'a>(responses: &'a HashMap<String, Captured>, vars: &'a [Binding]) -> Iteration<'a> {
        Iteration {
            responses,
            followed: None,
            vars,
        }
    }

    /// One loop variable bound to `value`.
    fn bind(var: &str, value: &Value) -> Vec<Binding> {
        vec![Binding {
            var: var.to_string(),
            index: 0,
            value: value.clone(),
        }]
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
            .references(&file.requests[0], &[])
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
        let r = file
            .resolve(&file.requests[0], &at(&responses, &[]))
            .unwrap();
        assert_eq!(r.url, "http://a/next");
        assert_eq!(r.headers, [("Authorization".into(), "Bearer t0k".into())]);

        let err = file
            .resolve(&file.requests[0], &at(&HashMap::new(), &[]))
            .unwrap_err();
        assert_eq!(err.to_string(), "no response from \"login\" yet");
    }

    const LOOP: &str = "### each\n# @foreach item in {{login.response.body.$.items}}\nGET http://a/{{item.id}}?all={{item}}\nX-Name: {{ item['id'] }}\n";

    #[test]
    fn parses_foreach_directive() {
        let file = parse(LOOP).unwrap();
        let foreach = &file.requests[0].foreach[0];
        assert_eq!(foreach.var, "item");
        assert!(matches!(&foreach.source, Source::Response(r) if r.name == "login"));

        let file = parse("// @foreach x in {{a.response.body.$}}\nGET http://a\n").unwrap();
        assert_eq!(file.requests[0].foreach[0].var, "x");

        // `@foreachx` is just a comment.
        let file = parse("# @foreachx\nGET http://a\n").unwrap();
        assert!(file.requests[0].foreach.is_empty());

        // Several @foreach lines nest; a source can be an earlier variable.
        let file = parse(
            "# @foreach a in {{x.response.body.$}}\n# @foreach b in {{a['k'].list[0]}}\n# @foreach c in {{y.response.body.$.c}}\nGET http://a\n",
        )
        .unwrap();
        let vars: Vec<_> = file.requests[0]
            .foreach
            .iter()
            .map(|f| f.var.as_str())
            .collect();
        assert_eq!(vars, ["a", "b", "c"]);
        assert_eq!(
            file.requests[0].foreach[1].source,
            Source::Var {
                text: "a['k'].list[0]".into(),
                var: "a".into(),
                steps: vec![
                    Step::Key("k".into()),
                    Step::Key("list".into()),
                    Step::Index(0)
                ],
            }
        );
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
                "# @foreach x in {{$env LIST}}\nGET http://a\n",
                "line 1: @foreach must loop over a {{NAME.response.body.$.path}} reference or a loop variable",
            ),
            (
                "# @foreach x in {{list..a}}\nGET http://a\n",
                "line 1: invalid loop variable reference {{list..a}}",
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
                "# @foreach x in {{a.response.body.$}}\n# @foreach x in {{a.response.body.$}}\nGET http://a\n",
                "line 2: @foreach variable \"x\" is used twice",
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
            .references(&file.requests[0], &[])
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect();
        assert_eq!(names, ["login", "other"]);
    }

    #[test]
    fn loop_variable_checks() {
        let clash = parse(&format!("@item = 1\n{LOOP}")).unwrap();
        let err = clash.references(&clash.requests[0], &[]).unwrap_err();
        assert_eq!(
            err.to_string(),
            "@foreach variable \"item\" has the same name as a file variable"
        );

        let bad =
            parse("# @foreach item in {{a.response.body.$}}\nGET http://a/{{item..x}}\n").unwrap();
        let err = bad.references(&bad.requests[0], &[]).unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid loop variable reference {{item..x}}"
        );

        // Only visible in its own request, not inside file variables.
        let hidden =
            parse("@v = {{item}}\n# @foreach item in {{a.response.body.$}}\nGET http://a/{{v}}\n")
                .unwrap();
        let err = hidden.references(&hidden.requests[0], &[]).unwrap_err();
        assert_eq!(err.to_string(), "undefined variable: item");
    }

    /// The `(var index)` pairs of each iteration, or the error.
    fn iterations(
        file: &HttpFile,
        request: &Request,
        ctx: &Iteration,
    ) -> std::result::Result<Vec<String>, String> {
        file.iterations(request, ctx)
            .map(|all| {
                all.iter()
                    .map(|vars| {
                        let each: Vec<_> = vars
                            .iter()
                            .map(|b| format!("{}{}={}", b.var, b.index, b.value))
                            .collect();
                        each.join(" ")
                    })
                    .collect()
            })
            .map_err(|e| {
                let vars: Vec<_> = e
                    .vars
                    .iter()
                    .map(|b| format!("{}{}", b.var, b.index))
                    .collect();
                format!("[{}] {:#}", vars.join(" "), e.error)
            })
    }

    #[test]
    fn loop_iterations_and_elements() {
        let file = parse(LOOP).unwrap();
        let request = &file.requests[0];
        let responses = HashMap::from([("login".to_string(), captured(BODY))]);
        assert_eq!(
            iterations(&file, request, &at(&responses, &[])).unwrap(),
            [r#"item0={"id":7}"#, r#"item1={"id":8}"#]
        );

        let element = serde_json::json!({ "id": 8 });
        let r = file
            .resolve(request, &at(&responses, &bind("item", &element)))
            .unwrap();
        assert_eq!(r.url, r#"http://a/8?all={"id":8}"#);
        assert_eq!(r.headers, [("X-Name".into(), "8".into())]);

        let missing = serde_json::json!({ "name": "no id" });
        let err = file
            .resolve(request, &at(&responses, &bind("item", &missing)))
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "current element has no value at {{item.id}}"
        );

        let not_array =
            parse("# @foreach x in {{login.response.body.$.token}}\nGET http://a\n").unwrap();
        assert_eq!(
            iterations(&not_array, &not_array.requests[0], &at(&responses, &[])).unwrap_err(),
            "[] @foreach: {{login.response.body.$.token}} is not an array"
        );

        let plain = parse("GET http://a\n").unwrap();
        assert_eq!(
            iterations(&plain, &plain.requests[0], &at(&responses, &[])).unwrap(),
            [""]
        );
    }

    #[test]
    fn nested_loops_iterate_every_combination() {
        let responses = HashMap::from([
            (
                "wcs".to_string(),
                captured(
                    r#"{"data": [{"id": "a", "domains": ["x", "y"]}, {"id": "b", "domains": []}, {"id": "c", "domains": ["z"]}]}"#,
                ),
            ),
            ("nums".to_string(), captured("[1, 2]")),
        ]);
        let file = parse(concat!(
            "### inner\n",
            "# @foreach wc in {{wcs.response.body.$.data}}\n",
            "# @foreach d in {{wc.domains}}\n",
            "GET http://a/{{wc.id}}/{{d}}\n",
            "### product\n",
            "# @foreach wc in {{wcs.response.body.$.data}}\n",
            "# @foreach n in {{nums.response.body.$}}\n",
            "GET http://a/{{wc.id}}/{{n}}\n",
            "### bad\n",
            "# @foreach wc in {{wcs.response.body.$.data}}\n",
            "# @foreach d in {{wc.id}}\n",
            "GET http://a/\n",
        ))
        .unwrap();
        let ctx = at(&responses, &[]);
        let short = |r: &Request| -> Vec<String> {
            file.iterations(r, &ctx)
                .unwrap()
                .iter()
                .map(|vars| file.resolve(r, &at(&responses, vars)).unwrap().url)
                .collect()
        };
        assert_eq!(
            short(&file.requests[0]),
            ["http://a/a/x", "http://a/a/y", "http://a/c/z"]
        );
        assert_eq!(
            short(&file.requests[1]),
            [
                "http://a/a/1",
                "http://a/a/2",
                "http://a/b/1",
                "http://a/b/2",
                "http://a/c/1",
                "http://a/c/2"
            ]
        );
        let all = iterations(&file, &file.requests[0], &ctx).unwrap();
        assert_eq!(all[2], r#"wc2={"id":"c","domains":["z"]} d0="z""#);
        assert_eq!(
            iterations(&file, &file.requests[2], &ctx).unwrap_err(),
            "[wc0] @foreach: {{wc.id}} is not an array"
        );

        // A followed loop's variables and response are in scope.
        let file = parse(concat!(
            "### follow\n",
            "# @foreach d in {{per wc.response.body.$.domains}}\n",
            "GET http://a/{{wc.id}}/{{d}}\n",
        ))
        .unwrap();
        let per_wc = captured(r#"{"domains": ["p", "q"]}"#);
        let inherited = bind("wc", &serde_json::json!({"id": "a"}));
        let ctx = Iteration {
            responses: &responses,
            followed: Some(("per wc", &per_wc)),
            vars: &inherited,
        };
        let urls: Vec<_> = file
            .iterations(&file.requests[0], &ctx)
            .unwrap()
            .into_iter()
            .map(|own| {
                let vars: Vec<_> = inherited.iter().cloned().chain(own).collect();
                let ctx = Iteration { vars: &vars, ..ctx };
                file.resolve(&file.requests[0], &ctx).unwrap().url
            })
            .collect();
        assert_eq!(urls, ["http://a/a/p", "http://a/a/q"]);
    }

    #[test]
    fn loop_variables_in_scope() {
        let file = parse(concat!(
            "### follow\n",
            "# @foreach d in {{wc.domains}}\n",
            "# @assert {{wc.id}} == \"{{d}}\"\n",
            "GET http://a/{{wc.id}}/{{d}}\n",
        ))
        .unwrap();
        let request = &file.requests[0];
        // Before the followed loop is known, names come without errors.
        assert!(file.referenced_names(request).unwrap().is_empty());
        let err = file.references(request, &[]).unwrap_err();
        assert_eq!(
            err.to_string(),
            "@foreach must loop over a {{NAME.response.body.$.path}} reference or an earlier loop variable: {{wc.domains}}"
        );
        assert!(file.references(request, &["wc".to_string()]).is_ok());
        let err = file
            .references(request, &["wc".to_string(), "d".to_string()])
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "@foreach variable \"d\" is already a loop variable"
        );

        // A later loop variable is not a source for an earlier @foreach.
        let file = parse(
            "# @foreach a in {{b.x}}\n# @foreach b in {{list.response.body.$}}\nGET http://a/\n",
        )
        .unwrap();
        let err = file.references(&file.requests[0], &[]).unwrap_err();
        assert!(
            err.to_string().ends_with("earlier loop variable: {{b.x}}"),
            "{err}"
        );

        // Names still include references found through file variables.
        let file = parse(
            "@v = {{login.response.body.$.v}}\n### r\n# @foreach d in {{wc.list}}\nGET http://a/{{v}}/{{wc.id}}/{{other.response.headers.X}}\n",
        )
        .unwrap();
        assert_eq!(
            file.referenced_names(&file.requests[0]).unwrap(),
            ["login", "other"]
        );
    }

    /// Resolves `GET http://a/{{expr}}` against `captured(BODY)` as "login",
    /// returning the substituted path.
    fn filtered(variables: &str, expr: &str) -> Result<String> {
        let file = parse(&format!("{variables}GET http://a/{{{{{expr}}}}}\n"))?;
        let responses = HashMap::from([("login".to_string(), captured(BODY))]);
        file.references(&file.requests[0], &[])?;
        let r = file.resolve(&file.requests[0], &at(&responses, &[]))?;
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
        let err = format!("{:#}", file.references(&file.requests[0], &[]).unwrap_err());
        assert!(err.contains("missing \")\""), "{err}");

        let file = parse(&format!(
            "{LOOP}X-Last: {{{{ item | split(\":\", -1) | prefix(\"\") }}}}\n"
        ))
        .unwrap();
        let responses = HashMap::from([("login".to_string(), captured(BODY))]);
        let element = serde_json::json!({ "id": 7 });
        let r = file
            .resolve(&file.requests[0], &at(&responses, &bind("item", &element)))
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
            .references(&file.requests[0], &[])
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
    fn variables_use_the_last_definition() {
        let url = |contents: &str| resolved(contents).map(|r| r.url).map_err(|e| e.to_string());
        // A variable can use one defined later.
        assert_eq!(
            url("@base = https://{{host}}\n@host = a\nGET {{base}}\n"),
            Ok("https://a".into())
        );
        // Late binding: a redefinition also applies inside earlier variables.
        assert_eq!(
            url("@base = a\n@api = {{base}}/v3\n@base = b\nGET http://{{api}}\n"),
            Ok("http://b/v3".into())
        );
        // A variable used in its own value takes the definition before it.
        assert_eq!(
            url("@p = a\n@p = {{p}}/b\n@p = {{p}}/c\nGET http://x/{{p}}\n"),
            Ok("http://x/a/b/c".into())
        );
        assert_eq!(
            url("@p = {{p}}/b\nGET http://x/{{p}}\n"),
            Err("undefined variable: p".into())
        );
        // A cycle between variables ends at an undefined one, as above.
        assert_eq!(
            url("@a = {{b}}\n@b = {{a}}\nGET http://x/{{a}}\n"),
            Err("undefined variable: a".into())
        );
        assert_eq!(
            url("@a = 1\n@a = {{b}}\n@b = {{a}}2\nGET http://x/{{a}}\n"),
            Ok("http://x/12".into())
        );
        assert_eq!(
            url("@a = 1\n@a = {{b}}\n@b = {{a}}2\nGET http://x/{{b}}\n"),
            Err("undefined variable: b".into())
        );
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
        file.references(request, &[]).unwrap();
        let mut responses = HashMap::new();
        responses.insert("other".to_string(), captured(r#"{"id": 7}"#));
        let vars = element
            .map(|element| bind(&request.foreach[0].var, element))
            .unwrap_or_default();
        file.check(request, own, &at(&responses, &vars))
            .into_iter()
            .map(|r| match r.error {
                None => format!("ok {}", r.text),
                Some(error) => format!(
                    "{} {}: {error}",
                    directive_keyword(r.level, r.inverted),
                    r.text
                ),
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
        assert_eq!(checks[0].expected.first().map(String::as_str), Some("200"));
        assert_eq!(
            (
                checks[1].level,
                checks[1].op,
                checks[1].expected.first().map(String::as_str)
            ),
            (Level::Warn, Op::Contains, Some("js\"on"))
        );
        assert_eq!(
            (
                checks[2].level,
                checks[2].op,
                checks[2].expected.first().map(String::as_str)
            ),
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
            .references(&file.requests[0], &[])
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
            let err = format!(
                "{:#}",
                file.references(&file.requests[0], &[]).err().unwrap()
            );
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
        let err = file.references(&file.requests[0], &[]).err().unwrap();
        assert_eq!(
            format!("{err:#}"),
            "@assert {{s}} == 200: undefined variable: response.status"
        );
    }

    #[test]
    fn parses_if_checks() {
        let file = parse(concat!(
            "# @fail-if status >= 500\n",
            "// @warn-if {{response.body.$.x}} == null\n",
            "# @notice-if {{response.body.$.x}} exists\n",
            "# @assert status == 200\n",
            "GET http://a/\n",
        ))
        .unwrap();
        let checks: Vec<_> = file.requests[0]
            .checks
            .iter()
            .map(|c| (c.level, c.inverted, c.directive()))
            .collect();
        assert_eq!(
            checks,
            [
                (Level::Assert, true, "@fail-if".to_string()),
                (Level::Warn, true, "@warn-if".to_string()),
                (Level::Notice, true, "@notice-if".to_string()),
                (Level::Assert, false, "@assert".to_string()),
            ]
        );
        let err = parse("# @warn-if status\nGET http://a/\n").err().unwrap();
        assert_eq!(
            format!("{err:#}"),
            "line 1: invalid @warn-if (missing operator): @warn-if status"
        );
        let err = parse("GET http://a/\n# @fail-if status == 1\n")
            .err()
            .unwrap();
        assert_eq!(
            err.to_string(),
            "line 2: @fail-if must come before the request line"
        );
        // `@warn-iffy` is not a check, just a comment.
        let file = parse("# @warn-iffy x\nGET http://a/\n").unwrap();
        assert!(file.requests[0].checks.is_empty());
    }

    #[test]
    fn if_checks_report_when_true() {
        let mut own = captured(r#"{"name": "web", "n": null, "count": 3}"#);
        own.status = 503;
        let results = run_checks(
            concat!(
                "# @fail-if status >= 500\n",
                "# @fail-if status < 500\n",
                "# @warn-if {{response.body.$.n}} == null\n",
                "# @warn-if {{response.body.$.name}} == null\n",
                "# @warn-if {{response.body.$.missing}} == null\n",
                "# @warn-if {{response.body.$.name}} > 2\n",
                "# @notice-if {{response.body.$.count}} > 2\n",
                "# @notice-if {{response.body.$.name}} contains \"we\"\n",
                "# @fail-if {{response.body.$.name}} exists\n",
                "# @fail-if {{response.body.$.missing}} exists\n",
                "# @fail-if {{response.body.$.missing}} !exists\n",
                "# @fail-if {{response.body.$.name}} !exists\n",
                "GET http://a/\n",
            ),
            &own,
            None,
        );
        assert_eq!(
            results,
            [
                "fail-if status >= 500: matched \"503\"",
                "ok status < 500",
                "warn-if {{response.body.$.n}} == null: matched \"null\"",
                "ok {{response.body.$.name}} == null",
                "warn-if {{response.body.$.missing}} == null: response body has no value at $.missing",
                "warn-if {{response.body.$.name}} > 2: not a number: \"web\"",
                "notice-if {{response.body.$.count}} > 2: matched \"3\"",
                "notice-if {{response.body.$.name}} contains \"we\": matched \"web\"",
                "fail-if {{response.body.$.name}} exists: matched \"web\"",
                "ok {{response.body.$.missing}} exists",
                "fail-if {{response.body.$.missing}} !exists: response body has no value at $.missing",
                "ok {{response.body.$.name}} !exists",
            ]
        );
    }

    #[test]
    fn let_variables_are_request_local() {
        let file = parse(concat!(
            "@base = http://a\n",
            "### each\n",
            "# @foreach wc in {{login.response.body.$.items}}\n",
            "# @let service = services_{{wc.id}}\n",
            "// @let  path = {{service | prefix(\"services_\")}}/{{other.response.headers.X}}  \n",
            "# @let empty =\n",
            "# @assert {{service}} contains \"_7\"\n",
            "GET {{base}}/{{path}}\n",
            "X-Service: {{service}}{{empty}}\n",
            "### other request\n",
            "GET {{base}}/{{service}}\n",
        ))
        .unwrap();
        let request = &file.requests[0];
        let names: Vec<_> = request.lets.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, ["service", "path", "empty"]);

        // A response used in a @let is a dependency.
        let refs: Vec<_> = file
            .references(request, &[])
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect();
        assert_eq!(refs, ["login", "other"]);

        let mut other = captured("{}");
        other.headers = vec![("X".into(), "x".into())];
        let responses = HashMap::from([
            ("login".to_string(), captured(BODY)),
            ("other".to_string(), other),
        ]);
        let element = serde_json::json!({ "id": 7 });
        let vars = bind("wc", &element);
        let r = file.resolve(request, &at(&responses, &vars)).unwrap();
        assert_eq!(r.url, "http://a/7/x");
        assert_eq!(r.headers, [("X-Service".into(), "services_7".into())]);
        let results = file.check(request, &captured("{}"), &at(&responses, &vars));
        assert_eq!(results[0].error, None);

        // Not visible in other requests.
        let err = file.references(&file.requests[1], &[]).unwrap_err();
        assert_eq!(err.to_string(), "undefined variable: service");
    }

    #[test]
    fn invalid_let_variables() {
        for (contents, expected) in [
            (
                "# @let x\nGET http://a\n",
                "line 1: invalid @let (expected \"@let NAME = VALUE\"): @let x",
            ),
            (
                "# @let a.b = 1\nGET http://a\n",
                "line 1: invalid @let variable (use letters, digits, _ and -): a.b",
            ),
            (
                "# @let = 1\nGET http://a\n",
                "line 1: invalid @let variable",
            ),
            (
                "# @let a = 1\n# @let a = 2\nGET http://a\n",
                "line 2: @let variable \"a\" is defined twice",
            ),
            (
                "GET http://a\n# @let a = 1\n",
                "line 2: @let must come before the request line",
            ),
        ] {
            let err = format!("{:#}", parse(contents).err().unwrap());
            assert!(err.starts_with(expected), "{contents}: {err}");
        }
        // `@letter` is just a comment.
        assert!(
            parse("# @letter = 1\nGET http://a\n").unwrap().requests[0]
                .lets
                .is_empty()
        );

        for (contents, expected) in [
            (
                "@a = 1\n# @let a = 2\nGET http://a\n",
                "@let variable \"a\" has the same name as a file variable",
            ),
            (
                "# @foreach a in {{x.response.body.$}}\n# @let a = 2\nGET http://a\n",
                "@let variable \"a\" has the same name as a loop variable",
            ),
            (
                "# @let a = {{nope}}\nGET http://a\n",
                "@let a: undefined variable: nope",
            ),
            // A @let only sees earlier ones, and file variables never see it.
            (
                "# @let a = {{b}}\n# @let b = 1\nGET http://a\n",
                "@let a: undefined variable: b",
            ),
            (
                "@f = {{a}}\n### r\n# @let a = 1\nGET http://a/{{f}}\n",
                "undefined variable: a",
            ),
        ] {
            let file = parse(contents).unwrap();
            let err = format!(
                "{:#}",
                file.references(file.requests.last().unwrap(), &[])
                    .unwrap_err()
            );
            assert_eq!(err, expected, "{contents}");
        }
        // A loop variable inherited from a followed loop also clashes.
        let file = parse("# @let wc = 1\nGET http://a\n").unwrap();
        let err = file
            .references(&file.requests[0], &["wc".to_string()])
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "@let variable \"wc\" has the same name as a loop variable"
        );
    }

    #[test]
    fn skip_if_conditions() {
        let file = parse(concat!(
            "### each\n",
            "# @foreach wc in {{login.response.body.$.items}}\n",
            "# @let service = services_{{wc.id}}\n",
            "# @skip-if {{service}} contains \"8\"\n",
            "// @skip-if {{wc.disabled}} exists\n",
            "GET http://a/{{wc.id}}\n",
        ))
        .unwrap();
        let request = &file.requests[0];
        assert_eq!(request.skips.len(), 2);
        assert_eq!(request.skips[0].directive(), "@skip-if");
        file.references(request, &[]).unwrap();

        let responses = HashMap::new();
        let skip = |element: Value| {
            let vars = bind("wc", &element);
            file.skip(request, &at(&responses, &vars))
                .map_err(|e| format!("{e:#}"))
        };
        assert_eq!(skip(serde_json::json!({"id": 7})), Ok(false));
        assert_eq!(skip(serde_json::json!({"id": 8})), Ok(true));
        assert_eq!(
            skip(serde_json::json!({"id": 7, "disabled": false})),
            Ok(true)
        );

        // An unreadable value is an error, except with exists.
        let file = parse(
            "# @foreach wc in {{a.response.body.$}}\n# @skip-if {{wc.n}} > 1\nGET http://a/\n",
        )
        .unwrap();
        let vars = bind("wc", &serde_json::json!({}));
        let err = file
            .skip(&file.requests[0], &at(&responses, &vars))
            .unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "@skip-if {{wc.n}} > 1: current element has no value at {{wc.n}}"
        );
    }

    #[test]
    fn invalid_skip_if() {
        let err = parse("GET http://a\n# @skip-if status == 1\n")
            .err()
            .unwrap();
        assert_eq!(
            err.to_string(),
            "line 2: @skip-if must come before the request line"
        );
        let err = parse("# @skip-if {{x}} ~ 1\nGET http://a\n").err().unwrap();
        assert!(
            format!("{err:#}").starts_with("line 1: invalid @skip-if (unknown operator"),
            "{err:#}"
        );
        // `@skip-iffy` is just a comment.
        assert!(
            parse("# @skip-iffy\nGET http://a\n").unwrap().requests[0]
                .skips
                .is_empty()
        );

        for (contents, inherited, expected) in [
            (
                "# @skip-if {{a}} == 1\nGET http://a\n",
                &[][..],
                "@skip-if is only for a looping request (@foreach)",
            ),
            (
                "# @foreach x in {{a.response.body.$}}\n# @skip-if status == 404\nGET http://a\n",
                &[][..],
                "@skip-if status == 404: cannot read {{response...}}, since nothing has been sent yet",
            ),
            (
                "# @skip-if {{ response.body.$.x }} exists\nGET http://a\n",
                &["wc".to_string()][..],
                "@skip-if {{ response.body.$.x }} exists: cannot read {{response...}}, since nothing has been sent yet",
            ),
            (
                "# @foreach x in {{a.response.body.$}}\n# @skip-if {{nope}} exists\nGET http://a\n",
                &[][..],
                "@skip-if {{nope}} exists: undefined variable: nope",
            ),
        ] {
            let file = parse(contents).unwrap();
            let err = format!(
                "{:#}",
                file.references(&file.requests[0], inherited).unwrap_err()
            );
            assert_eq!(err, expected, "{contents}");
        }
        // Following a loop makes a request loop, so it may skip.
        let file = parse("# @skip-if {{wc.id}} == 1\nGET http://a\n").unwrap();
        assert!(
            file.references(&file.requests[0], &["wc".to_string()])
                .is_ok()
        );
    }

    #[test]
    fn in_compares_with_a_list() {
        assert_eq!(
            split_list(r#"200, "a,b" , {{x | split(",", 1)}},"q\"," "#),
            ["200", r#""a,b""#, r#"{{x | split(",", 1)}}"#, r#""q\",""#]
        );

        let mut own = captured(r#"{"state": "active", "n": 2, "path": "/a/b"}"#);
        own.status = 404;
        let results = run_checks(
            concat!(
                "@b = b\n",
                "# @assert status in 200, 404\n",
                "# @assert status in 200,201\n",
                "# @assert status !in 200, 201\n",
                "# @fail-if status !in 200, 404\n",
                "# @fail-if status in 500, 404\n",
                "# @assert {{response.body.$.state}} in \"pending\", \"active\"\n",
                "# @assert {{response.body.$.n}} in 1, 2\n",
                "# @assert {{response.body.$.path | split(\"/\", -1)}} in a, {{b}}\n",
                "# @assert {{response.body.$.nope}} in 1\n",
                "# @assert {{response.body.$.nope}} !in 1\n",
                "GET http://a/\n",
            ),
            &own,
            None,
        );
        assert_eq!(
            results,
            [
                "ok status in 200, 404",
                "assert status in 200,201: got \"404\"",
                "ok status !in 200, 201",
                "ok status !in 200, 404",
                "fail-if status in 500, 404: matched \"404\"",
                "ok {{response.body.$.state}} in \"pending\", \"active\"",
                "ok {{response.body.$.n}} in 1, 2",
                "ok {{response.body.$.path | split(\"/\", -1)}} in a, {{b}}",
                "assert {{response.body.$.nope}} in 1: response body has no value at $.nope",
                "assert {{response.body.$.nope}} !in 1: response body has no value at $.nope",
            ]
        );

        for (line, message) in [
            ("@assert status in", "in needs a value"),
            ("@assert status in 200,", "in has an empty value"),
            ("@assert status !in 200, , 201", "!in has an empty value"),
            (
                "@assert status in \"200\" x, 201",
                "unexpected text after the closing quote: x",
            ),
        ] {
            let err = format!(
                "{:#}",
                parse(&format!("# {line}\nGET http://a/\n")).err().unwrap()
            );
            assert!(err.contains(message), "{line}: {err}");
        }
        // Every value is checked like any other text.
        let file = parse("# @assert status in 200, {{nope}}\nGET http://a/\n").unwrap();
        let err = format!("{:#}", file.references(&file.requests[0], &[]).unwrap_err());
        assert_eq!(
            err,
            "@assert status in 200, {{nope}}: undefined variable: nope"
        );
    }

    /// A fresh directory holding `files` (relative path, contents).
    fn files(test: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("axon-httpfile-{}-{test}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (path, contents) in files {
            let path = dir.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
        dir
    }

    fn shown(path: PathBuf) -> String {
        path.display().to_string()
    }

    #[test]
    fn imports_bring_variables_and_requests() {
        let dir = files(
            "imports",
            &[
                (
                    "common.http",
                    "@base = http://common\n@sysapi = {{base}}/v3\n\n### get cluster\nGET {{sysapi}}/clusters/{{cluster}}?base={{base}}\n",
                ),
                (
                    "main.http",
                    "# shared requests\nimport common.http\n@base = http://main\n@cluster = cf\n\n### webconfigs\nGET {{sysapi}}/{{get cluster.response.body.$.id}}\n",
                ),
            ],
        );
        let file = parse_file(&dir.join("main.http")).unwrap();
        let names: Vec<_> = file
            .requests
            .iter()
            .map(|r| r.name.clone().unwrap())
            .collect();
        assert_eq!(names, ["get cluster", "webconfigs"]);
        assert_eq!(file.requests[0].file, shown(dir.join("common.http")));
        assert_eq!(file.requests[1].file, shown(dir.join("main.http")));
        assert_eq!(
            (file.requests[0].start_line, file.requests[0].end_line),
            (4, 5)
        );

        // The imported request sees the importing file's variables, and the
        // later definition of @base wins everywhere, including inside the
        // imported @sysapi (late binding).
        let r = file
            .resolve(&file.requests[0], &at(&HashMap::new(), &[]))
            .unwrap();
        assert_eq!(r.url, "http://main/v3/clusters/cf?base=http://main");
        let names: Vec<_> = file
            .references(&file.requests[1], &[])
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect();
        assert_eq!(names, ["get cluster"]);
    }

    #[test]
    fn imports_nest_and_include_each_file_once() {
        let dir = files(
            "imports-nested",
            &[
                ("shared.http", "@v = 1\n### shared\nGET http://a/{{v}}\n"),
                (
                    "sub/b.http",
                    "import ../shared.http\n### b\nGET http://a/b\n",
                ),
                ("a.http", "import shared.http\n### a\nGET http://a/a\n"),
                (
                    "main.http",
                    "import a.http\nimport sub/b.http\n### main\nGET http://a/main\n",
                ),
                ("vars.http", "@only = vars\n"),
                (
                    "uses-vars.http",
                    "import vars.http\nGET http://a/{{only}}\n",
                ),
            ],
        );
        let file = parse_file(&dir.join("main.http")).unwrap();
        let names: Vec<_> = file
            .requests
            .iter()
            .map(|r| r.name.clone().unwrap())
            .collect();
        assert_eq!(names, ["shared", "a", "b", "main"]);
        assert_eq!(file.requests[2].file, shown(dir.join("sub/b.http")));

        // A file with only variables is a valid import.
        let file = parse_file(&dir.join("uses-vars.http")).unwrap();
        let r = file
            .resolve(&file.requests[0], &at(&HashMap::new(), &[]))
            .unwrap();
        assert_eq!(r.url, "http://a/vars");
    }

    #[test]
    fn import_errors_name_the_file() {
        let dir = files(
            "imports-errors",
            &[
                ("missing.http", "import nope.http\nGET http://a\n"),
                ("bad.http", "@ok = 1\nnot a valid request line here\n"),
                ("uses-bad.http", "import bad.http\nGET http://a\n"),
                ("x.http", "import y.http\n"),
                ("y.http", "\nimport x.http\n"),
                (
                    "late.http",
                    "### a\nGET http://a\n### b\nimport x.http\nGET http://b\n",
                ),
                ("in-headers.http", "GET http://a\nimport x.http\n"),
                ("empty.http", "import\n"),
            ],
        );
        let err = |name: &str| format!("{:#}", parse_file(&dir.join(name)).err().unwrap());
        let path = |name: &str| shown(dir.join(name));

        let missing = err("missing.http");
        let expected = format!(
            "failed to read {} (imported by {} line 1): ",
            path("nope.http"),
            path("missing.http")
        );
        assert!(missing.starts_with(&expected), "{missing}");
        assert!(
            err("uses-bad.http").starts_with(&format!(
                "failed to parse {}: line 2: invalid request line",
                path("bad.http")
            )),
            "{}",
            err("uses-bad.http")
        );
        assert_eq!(
            err("x.http"),
            format!(
                "failed to parse {}: line 2: import cycle: {} -> {} -> {}",
                path("y.http"),
                path("x.http"),
                path("y.http"),
                path("x.http")
            )
        );
        for (name, line) in [("late.http", 4), ("in-headers.http", 2)] {
            assert_eq!(
                err(name),
                format!(
                    "failed to parse {}: line {line}: import must come before the first request",
                    path(name)
                )
            );
        }
        assert_eq!(
            err("empty.http"),
            format!(
                "failed to parse {}: line 1: import needs a path",
                path("empty.http")
            )
        );
        assert_eq!(
            format!("{:#}", parse_file(&dir.join("absent.http")).err().unwrap()),
            format!(
                "failed to read {}: No such file or directory (os error 2)",
                path("absent.http")
            )
        );
        // A line that only starts with "import" is not an import.
        let file = parse("imports.example.com/x\n").unwrap();
        assert_eq!(file.requests[0].url, "imports.example.com/x");
    }
}
