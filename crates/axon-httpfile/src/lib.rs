//! Parser for `.http` files (JetBrains / VS Code REST Client format):
//! `###` separators, `# @name`, `@var = value` file variables, and
//! `{{var}}` / `{{$env NAME}}` substitution.

use anyhow::{Context, Result, bail};

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
    /// 1-based lines of the request's block: from its `###` separator (or the
    /// start of the file) up to the line before the next separator.
    pub start_line: usize,
    pub end_line: usize,
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

        // Variables, comments and `# @name`, up to the request line.
        let (line_no, request_line) = loop {
            let Some(&(line_no, line)) = lines.next() else {
                return Ok(());
            };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some(directive) = name_directive(line) {
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
            start_line,
            end_line,
        });
        Ok(())
    }

    /// Substitutes variables in the request's URL, headers and body.
    pub fn resolve(&self, request: &Request) -> Result<Request> {
        let all = self.variables.len();
        let sub = |text: &str| self.substitute(text, all);
        Ok(Request {
            name: request.name.clone(),
            method: request.method.clone(),
            url: sub(&request.url)?,
            headers: request
                .headers
                .iter()
                .map(|(name, value)| Ok((sub(name)?, sub(value)?)))
                .collect::<Result<_>>()?,
            body: request.body.as_deref().map(sub).transpose()?,
            start_line: request.start_line,
            end_line: request.end_line,
        })
    }

    /// Replaces each `{{...}}` in `text`, seeing only the first `visible`
    /// file variables (so a variable can only reference earlier ones).
    fn substitute(&self, text: &str, visible: usize) -> Result<String> {
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(start) = rest.find("{{") {
            out.push_str(&rest[..start]);
            let after = &rest[start + 2..];
            let Some(end) = after.find("}}") else {
                bail!("unterminated \"{{{{\" in: {text}");
            };
            out.push_str(&self.lookup(after[..end].trim(), visible)?);
            rest = &after[end + 2..];
        }
        out.push_str(rest);
        Ok(out)
    }

    fn lookup(&self, name: &str, visible: usize) -> Result<String> {
        if let Some(dynamic) = name.strip_prefix('$') {
            let Some(var) = dynamic
                .strip_prefix("env")
                .filter(|v| v.starts_with(char::is_whitespace))
            else {
                bail!("unsupported dynamic variable: {{{{{name}}}}}");
            };
            let var = var.trim();
            return std::env::var(var)
                .with_context(|| format!("environment variable not set: {var}"));
        }
        // A redefined variable takes its latest earlier definition.
        let index = self.variables[..visible]
            .iter()
            .rposition(|(n, _)| n == name)
            .with_context(|| format!("undefined variable: {name}"))?;
        self.substitute(&self.variables[index].1, index)
    }
}

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
            start_line,
            end_line,
        }
    }

    fn resolved(contents: &str) -> Result<Request> {
        let file = parse(contents).unwrap();
        file.resolve(&file.requests[0])
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
}
