use std::collections::{HashMap, HashSet};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result, bail};
use axon_httpfile as httpfile;
use axon_netrc as netrc;
use base64::Engine;
use clap::Parser;
use reqwest::blocking::{Client, RequestBuilder, Response};
use reqwest::cookie::Cookie;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};
use reqwest::redirect::Policy;
use reqwest::{Method, StatusCode, Url, Version};
use serde_json::{Map, Value, json};

/// Runs named requests from a .http file, authenticating via .netrc, and prints
/// the responses: status line and headers curl-style, then the raw body (or,
/// with --json, the full responses as JSON).
#[derive(Parser)]
#[command(version)]
struct Args {
    /// .http file to read requests from
    #[arg(short, long, value_name = "FILE")]
    file: PathBuf,

    /// Name of a request to run, or `#N` for the Nth request in the file
    /// (repeatable, run in the order given); without it, the requests in the
    /// file are listed
    #[arg(short, long = "name", value_name = "NAME")]
    names: Vec<String>,

    /// List only the request names, as a JSON array of strings (`#N` for an
    /// unnamed request)
    #[arg(short, long, conflicts_with = "names")]
    list: bool,

    /// netrc file to read credentials from [default: ~/.netrc]
    #[arg(long, value_name = "PATH")]
    netrc_file: Option<PathBuf>,

    /// Do not follow redirects
    #[arg(long)]
    no_follow: bool,

    /// Print each response as JSON (status, headers, cookies and body)
    /// instead of the headers and raw body
    #[arg(short, long)]
    json: bool,

    /// Print only the raw response body, without the status line and headers
    #[arg(short, long, conflicts_with = "json")]
    body: bool,

    /// With --json, also print the responses of requests run only as
    /// dependencies, marked `"dependency": true`; the output is then always
    /// an array of every response, in the order sent
    #[arg(long, requires = "json")]
    with_deps: bool,

    /// Treat a failed `@warn` check like a failed `@assert`
    #[arg(long)]
    strict: bool,
}

enum Output {
    Json(Value),
    /// Responses already written to stdout as they arrived.
    Written,
}

fn main() -> ExitCode {
    match run(Args::parse()) {
        Ok((output, failed)) => {
            if let Output::Json(output) = output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&output).expect("JSON value serializes")
                );
            }
            match failed {
                Some(summary) => {
                    eprintln!("{}", json!({ "error": summary }));
                    ExitCode::FAILURE
                }
                None => ExitCode::SUCCESS,
            }
        }
        Err(err) => {
            eprintln!("{}", json!({ "error": format!("{err:#}") }));
            ExitCode::FAILURE
        }
    }
}

/// Runs the requests, returning the output and, when checks failed, a
/// summary such as `2 checks failed, 1 request skipped`.
fn run(args: Args) -> Result<(Output, Option<String>)> {
    let path = &args.file;
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let file = httpfile::parse(&contents)
        .with_context(|| format!("failed to parse {}", path.display()))?;

    if args.list {
        // An unnamed request is listed as the `#N` that selects it with -n.
        return Ok((
            Output::Json(
                file.requests
                    .iter()
                    .zip(1..)
                    .map(|(r, index)| r.name.clone().unwrap_or(format!("#{index}")))
                    .collect(),
            ),
            None,
        ));
    }
    if args.names.is_empty() {
        return Ok((
            Output::Json(
                file.requests
                    .iter()
                    .zip(1..)
                    .map(|(r, index)| {
                        json!({
                            "index": index,
                            "name": r.name,
                            "method": r.method,
                            "url": r.url,
                            "start_line": r.start_line,
                            "end_line": r.end_line,
                        })
                    })
                    .collect(),
            ),
            None,
        ));
    }

    // Plan every request to send and build each one that needs no earlier
    // response, so bad input fails before anything is sent.
    let mut steps = plan(&file, &args.names)?;
    let netrc = read_netrc(args.netrc_file.as_deref())?;
    let client = Client::builder()
        .user_agent(concat!(
            env!("CARGO_PKG_NAME"),
            "/",
            env!("CARGO_PKG_VERSION")
        ))
        .redirect(if args.no_follow {
            Policy::none()
        } else {
            Policy::default()
        })
        .cookie_store(true)
        .build()?;

    let no_responses = HashMap::new();
    let alone = httpfile::Iteration {
        responses: &no_responses,
        followed: None,
        vars: &[],
    };
    for step in steps.iter_mut().filter(|step| !step.chained) {
        step.builder = Some(
            file.resolve(step.request, &alone)
                .and_then(|request| build(&client, request, netrc.as_deref()))
                .with_context(|| format!("request \"{}\"", step.label))?,
        );
    }

    let mut responses = HashMap::new();
    // The iterations of each looping request, by name, for the requests
    // that follow it.
    let mut loops: HashMap<String, Vec<LoopRun>> = HashMap::new();
    let mut json_output = Vec::new();
    let mut stdout = std::io::stdout().lock();
    let mut last_byte = None;
    // Requests whose latest run failed a check that counts, or was skipped.
    let mut failed = HashSet::new();
    let mut failed_checks = 0;
    let mut skipped = 0;
    for mut step in steps {
        let label = format!("request \"{}\"", step.label);
        if let Some(dependency) = step.depends_on.iter().find(|d| failed.contains(*d)) {
            eprintln!("SKIP {label}: \"{dependency}\" failed its checks");
            skipped += 1;
            if let Some(name) = &step.request.name {
                failed.insert(name.clone());
            }
            if args.json && (step.selected || args.with_deps) {
                json_output.push(Value::Null);
            }
            continue;
        }
        let mut iterations = Vec::new();
        let mut runs = Vec::new();
        let mut step_failed = false;
        // A request that follows a looping request runs for each of its
        // iterations; any other request once, plus its own @foreach.
        let outer: Vec<Option<&LoopRun>> = match &step.follows {
            Some(name) => loops
                .get(name)
                .map(|runs| runs.iter().map(Some).collect())
                .unwrap_or_default(),
            None => vec![None],
        };
        for run in outer {
            let inherited = run.map_or(&[][..], |run| run.vars.as_slice());
            if let Some(run) = run
                && run.failed
            {
                let followed = step.follows.as_deref().unwrap_or_default();
                let at = describe(&run.vars);
                eprintln!("SKIP {label}{at}: \"{followed}\"{at} failed its checks");
                skipped += 1;
                continue;
            }
            let followed = step.follows.as_deref().zip(run.map(|run| &run.captured));
            let combinations = file
                .iterations(
                    step.request,
                    &httpfile::Iteration {
                        responses: &responses,
                        followed,
                        vars: inherited,
                    },
                )
                .map_err(|err| {
                    let vars: Vec<_> = inherited.iter().cloned().chain(err.vars).collect();
                    err.error.context(format!("{label}{}", describe(&vars)))
                })?;
            for own in combinations {
                let vars: Vec<httpfile::Binding> = inherited.iter().cloned().chain(own).collect();
                let at = describe(&vars);
                let label = || format!("{label}{at}");
                let ctx = httpfile::Iteration {
                    responses: &responses,
                    followed,
                    vars: &vars,
                };
                // `@skip-if`: this iteration is not sent, like `continue`.
                if file.skip(step.request, &ctx).with_context(label)? {
                    continue;
                }
                let builder = match step.builder.take() {
                    Some(builder) => builder,
                    None => file
                        .resolve(step.request, &ctx)
                        .and_then(|request| build(&client, request, netrc.as_deref()))
                        .with_context(label)?,
                };
                let received = builder
                    .send()
                    .map_err(anyhow::Error::from)
                    .and_then(Received::read)
                    .with_context(label)?;
                let captured = received.captured();
                let checks = file.check(step.request, &captured, &ctx);

                if args.json {
                    if step.selected || args.with_deps {
                        let mut output = received.json(step.index, step.request.name.as_deref());
                        if !step.selected {
                            output["dependency"] = json!(true);
                        }
                        if step.looping {
                            output["loop"] = loop_json(&vars);
                        }
                        if !step.request.checks.is_empty() {
                            output["checks"] = checks_json(&checks);
                        }
                        iterations.push(output);
                    }
                // -b prints only the -n requests; the default output prints every response.
                } else if !args.body || step.selected {
                    let mut output = Vec::new();
                    if !args.body {
                        // Start each later header block on its own line; bare bodies stay as sent.
                        if last_byte.is_some_and(|byte| byte != b'\n') {
                            output.push(b'\n');
                        }
                        output.extend_from_slice(received.head().as_bytes());
                    }
                    output.extend_from_slice(&received.body);
                    stdout
                        .write_all(&output)
                        .and_then(|()| stdout.flush())
                        .context("failed to write body")?;
                    last_byte = output.last().copied().or(last_byte);
                }

                // Failed checks are reported after the response they belong to.
                let mut iteration_failed = false;
                for check in &checks {
                    let Some(error) = &check.error else {
                        continue;
                    };
                    let tag = match check.level {
                        httpfile::Level::Assert => "FAIL",
                        httpfile::Level::Warn => "WARN",
                        httpfile::Level::Notice => "NOTICE",
                    };
                    // A `-if` check reads `if CHECK: matched ...`.
                    let condition = if check.inverted { "if " } else { "" };
                    eprintln!("{tag} {}: {condition}{}: {error}", label(), check.text);
                    if check.level == httpfile::Level::Assert
                        || (args.strict && check.level == httpfile::Level::Warn)
                    {
                        failed_checks += 1;
                        iteration_failed = true;
                    }
                }
                step_failed |= iteration_failed;

                // A looping request's iterations are kept for the requests
                // that follow it; any other response by name.
                if step.looping {
                    runs.push(LoopRun {
                        vars,
                        captured,
                        failed: iteration_failed,
                    });
                } else if let Some(name) = &step.request.name {
                    responses.insert(name.clone(), captured);
                }
            }
        }
        if let Some(name) = &step.request.name {
            // Failed iterations of a looping request only skip the
            // iterations that follow them.
            if step_failed && !step.looping {
                failed.insert(name.clone());
            } else {
                failed.remove(name);
            }
            if step.looping {
                loops.insert(name.clone(), runs);
            }
        }
        // A looping -n takes one JSON slot holding all of its iterations.
        if args.json && (step.selected || args.with_deps) {
            if step.looping {
                json_output.push(Value::Array(iterations));
            } else {
                json_output.extend(iterations);
            }
        }
    }

    let summary = (failed_checks + skipped > 0).then(|| {
        let mut summary = count(failed_checks, "check", "failed");
        if skipped > 0 {
            summary.push_str(&format!(", {}", count(skipped, "request", "skipped")));
        }
        summary
    });
    if !args.json {
        return Ok((Output::Written, summary));
    }
    let output = if args.names.len() == 1 && !args.with_deps {
        json_output.remove(0)
    } else {
        Value::Array(json_output)
    };
    Ok((Output::Json(output), summary))
}

/// One iteration of a looping request, for the requests that follow it.
struct LoopRun {
    vars: Vec<httpfile::Binding>,
    captured: httpfile::Captured,
    /// It failed a check that counts.
    failed: bool,
}

/// How messages name an iteration: ` (element 2)` with one loop variable,
/// ` (wc 2, domain 0)` with more, nothing without.
fn describe(vars: &[httpfile::Binding]) -> String {
    match vars {
        [] => String::new(),
        [one] => format!(" (element {})", one.index),
        _ => {
            let each: Vec<_> = vars
                .iter()
                .map(|binding| format!("{} {}", binding.var, binding.index))
                .collect();
            format!(" ({})", each.join(", "))
        }
    }
}

/// The index of each loop variable in an iteration, for `--json`.
fn loop_json(vars: &[httpfile::Binding]) -> Value {
    Value::Object(
        vars.iter()
            .map(|binding| (binding.var.clone(), json!(binding.index)))
            .collect(),
    )
}

/// `1 check failed`, `2 checks failed`.
fn count(n: usize, noun: &str, verb: &str) -> String {
    let plural = if n == 1 { "" } else { "s" };
    format!("{n} {noun}{plural} {verb}")
}

/// The results of a response's checks, passing ones included.
fn checks_json(checks: &[httpfile::CheckResult]) -> Value {
    checks
        .iter()
        .map(|check| {
            let mut output = json!({
                "level": check.level.keyword(),
                "check": check.text,
                "ok": check.error.is_none(),
            });
            if check.inverted {
                output["if"] = json!(true);
            }
            if let Some(error) = &check.error {
                output["error"] = json!(error);
            }
            output
        })
        .collect()
}

/// A request to send, in the order planned.
struct Step<'a> {
    /// 1-based position in the file.
    index: usize,
    request: &'a httpfile::Request,
    /// How errors name the request: its `-n` selector, or its name when it
    /// runs as a dependency.
    label: String,
    /// Given with `-n`, rather than run only as a dependency.
    selected: bool,
    /// Uses earlier responses, so it can only be built once those arrive.
    chained: bool,
    /// Names of the requests whose responses it uses; it is skipped when one
    /// of them failed its checks.
    depends_on: Vec<String>,
    /// The looping request it follows: it runs once per iteration of it.
    follows: Option<String>,
    /// Runs once per iteration: it has `@foreach` lines or follows a loop.
    looping: bool,
    builder: Option<RequestBuilder>,
}

/// Orders the selected requests and the requests they reference: each
/// dependency without a response yet runs first (depth-first), once; a
/// request given with `-n` always runs.
fn plan<'a>(file: &'a httpfile::HttpFile, selectors: &[String]) -> Result<Vec<Step<'a>>> {
    let mut planner = Planner {
        file,
        steps: Vec::new(),
        responded: HashSet::new(),
        stack: Vec::new(),
        loops: HashMap::new(),
    };
    for selector in selectors {
        let (index, request) = select(&file.requests, selector)?;
        planner
            .visit(index, request, selector, true)
            .with_context(|| format!("request \"{selector}\""))?;
    }
    Ok(planner.steps)
}

struct Planner<'a> {
    file: &'a httpfile::HttpFile,
    steps: Vec<Step<'a>>,
    /// Indexes of the requests planned so far.
    responded: HashSet<usize>,
    /// Indexes of the requests being visited, to detect cycles.
    stack: Vec<usize>,
    /// Whether each planned request loops, by index, with the loop
    /// variables in its scope (those of the loop it follows first).
    loops: HashMap<usize, (bool, Vec<String>)>,
}

impl<'a> Planner<'a> {
    fn visit(
        &mut self,
        index: usize,
        request: &'a httpfile::Request,
        label: &str,
        selected: bool,
    ) -> Result<()> {
        if !selected && self.responded.contains(&index) {
            return Ok(());
        }
        if let Some(start) = self.stack.iter().position(|&i| i == index) {
            let cycle: Vec<_> = self.stack[start..]
                .iter()
                .chain([&index])
                .map(|&i| {
                    self.file.requests[i - 1]
                        .name
                        .clone()
                        .unwrap_or(format!("#{i}"))
                })
                .collect();
            bail!("dependency cycle: {}", cycle.join(" -> "));
        }

        let names = self.file.referenced_names(request)?;
        self.stack.push(index);
        let mut follows: Option<(&String, usize)> = None;
        for name in &names {
            let (dep_index, dependency) = find_named(&self.file.requests, name)?
                .with_context(|| format!("no request named \"{name}\""))?;
            if dep_index == index {
                bail!("request \"{name}\" depends on itself");
            }
            self.visit(dep_index, dependency, name, false)
                .with_context(|| format!("request \"{name}\""))?;
            if self.loops[&dep_index].0 {
                if let Some((other, _)) = follows {
                    bail!("references more than one looping request: \"{other}\" and \"{name}\"");
                }
                follows = Some((name, dep_index));
            }
        }
        self.stack.pop();

        // Now that the loop it follows is known, check its variables.
        let mut vars = follows.map_or_else(Vec::new, |(_, i)| self.loops[&i].1.clone());
        self.file.references(request, &vars)?;
        vars.extend(request.foreach.iter().map(|foreach| foreach.var.clone()));
        let looping = !request.foreach.is_empty() || follows.is_some();
        self.loops.insert(index, (looping, vars));

        self.steps.push(Step {
            index,
            request,
            label: label.to_string(),
            selected,
            chained: !names.is_empty(),
            follows: follows.map(|(name, _)| name.clone()),
            looping,
            depends_on: names,
            builder: None,
        });
        self.responded.insert(index);
        Ok(())
    }
}

/// The one request with this exact name, with its 1-based index.
fn find_named<'a>(
    requests: &'a [httpfile::Request],
    name: &str,
) -> Result<Option<(usize, &'a httpfile::Request)>> {
    let mut matches = (1..)
        .zip(requests)
        .filter(|(_, r)| r.name.as_deref() == Some(name));
    let found = matches.next();
    if matches.next().is_some() {
        bail!("more than one request is named \"{name}\"");
    }
    Ok(found)
}

/// Finds the one request with this exact name or, failing that, the request
/// at a 1-based `#N` index, returning it with its index.
fn select<'a>(
    requests: &'a [httpfile::Request],
    selector: &str,
) -> Result<(usize, &'a httpfile::Request)> {
    if let Some(found) = find_named(requests, selector)? {
        return Ok(found);
    }
    let index = selector
        .strip_prefix('#')
        .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    let Some(index) = index else {
        bail!("no request named \"{selector}\"");
    };
    index
        .parse::<usize>()
        .ok()
        .and_then(|index| Some((index, requests.get(index.checked_sub(1)?)?)))
        .with_context(|| {
            format!(
                "no request {selector} (the file has {} requests)",
                requests.len()
            )
        })
}

fn build(
    client: &Client,
    request: httpfile::Request,
    netrc: Option<&str>,
) -> Result<RequestBuilder> {
    let url = Url::parse(&request.url).with_context(|| format!("invalid URL: {}", request.url))?;
    let method = Method::from_bytes(request.method.as_bytes())
        .with_context(|| format!("invalid method: {}", request.method))?;
    let headers = parse_headers(&request.headers)?;

    // Explicit credentials (Authorization header or user:pass@ in the URL) win over netrc.
    let credentials = if headers.contains_key(AUTHORIZATION) || !url.username().is_empty() {
        None
    } else {
        netrc
            .zip(url.host_str())
            .and_then(|(contents, host)| netrc::lookup(contents, host))
    };

    let mut builder = client.request(method, url).headers(headers);
    if let Some(creds) = credentials {
        builder = builder.basic_auth(creds.login, creds.password);
    }
    if let Some(body) = request.body {
        builder = builder.body(body);
    }
    Ok(builder)
}

fn parse_headers(raw: &[(String, String)]) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    for (name, value) in raw {
        let name = HeaderName::from_bytes(name.as_bytes())
            .with_context(|| format!("invalid header name: {name}"))?;
        let value = HeaderValue::from_str(value)
            .with_context(|| format!("invalid header value: {value}"))?;
        headers.append(name, value);
    }
    Ok(headers)
}

/// Reads the netrc file. A missing default `~/.netrc` is not an error, but a
/// missing file passed via `--netrc-file` is.
fn read_netrc(path: Option<&Path>) -> Result<Option<String>> {
    if let Some(path) = path {
        return std::fs::read_to_string(path)
            .map(Some)
            .with_context(|| format!("failed to read netrc file {}", path.display()));
    }
    let Some(home) = std::env::var_os("HOME") else {
        return Ok(None);
    };
    match std::fs::read_to_string(Path::new(&home).join(".netrc")) {
        Ok(contents) => Ok(Some(contents)),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).context("failed to read ~/.netrc"),
    }
}

/// A response read in full.
struct Received {
    url: String,
    version: Version,
    status: StatusCode,
    headers: HeaderMap,
    cookies: Vec<Value>,
    body: Vec<u8>,
}

impl Received {
    fn read(response: Response) -> Result<Self> {
        Ok(Received {
            url: response.url().to_string(),
            version: response.version(),
            status: response.status(),
            headers: response.headers().clone(),
            cookies: response.cookies().map(cookie_json).collect(),
            body: response
                .bytes()
                .context("failed to read response body")?
                .to_vec(),
        })
    }

    /// What later requests can reference.
    fn captured(&self) -> httpfile::Captured {
        httpfile::Captured {
            status: self.status.as_u16(),
            headers: self
                .headers
                .iter()
                .map(|(name, value)| {
                    let value = String::from_utf8_lossy(value.as_bytes()).into_owned();
                    (name.to_string(), value)
                })
                .collect(),
            body: self.body.clone(),
        }
    }

    fn json(&self, index: usize, name: Option<&str>) -> Value {
        let (base64, body) = body_text(&self.body);
        let mut output = json!({
            "index": index,
            "name": name,
            "url": self.url,
            "version": format!("{:?}", self.version),
            "status": { "code": self.status.as_u16(), "reason": self.status.canonical_reason() },
            "headers": headers_json(&self.headers),
            "cookies": self.cookies,
            "body": body,
        });
        if base64 {
            output["body_base64"] = json!(true);
        }
        output
    }

    /// The status line and headers like `curl -v`, each prefixed with `< `,
    /// ending with a `< ` line.
    fn head(&self) -> String {
        let mut head = format!("< {:?} {}", self.version, self.status.as_u16());
        if let Some(reason) = self.status.canonical_reason() {
            head.push_str(&format!(" {reason}"));
        }
        head.push('\n');
        for (name, value) in &self.headers {
            head.push_str(&format!(
                "< {name}: {}\n",
                String::from_utf8_lossy(value.as_bytes())
            ));
        }
        head.push_str("< \n");
        head
    }
}

/// Maps each header name to all of its values, in response order.
fn headers_json(headers: &HeaderMap) -> Value {
    let map: Map<String, Value> = headers
        .keys()
        .map(|name| {
            let values: Vec<String> = headers
                .get_all(name)
                .iter()
                .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
                .collect();
            (name.to_string(), json!(values))
        })
        .collect();
    Value::Object(map)
}

fn cookie_json(cookie: Cookie) -> Value {
    let same_site = if cookie.same_site_strict() {
        Some("Strict")
    } else if cookie.same_site_lax() {
        Some("Lax")
    } else {
        None
    };
    json!({
        "name": cookie.name(),
        "value": cookie.value(),
        "domain": cookie.domain(),
        "path": cookie.path(),
        "expires": cookie.expires().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs()),
        "max_age": cookie.max_age().map(|d| d.as_secs()),
        "secure": cookie.secure(),
        "http_only": cookie.http_only(),
        "same_site": same_site,
    })
}

/// The body exactly as received, as a string: the text itself when it is
/// UTF-8 (JSON included, unparsed), else base64-encoded, which the returned
/// flag reports.
fn body_text(body: &[u8]) -> (bool, String) {
    match std::str::from_utf8(body) {
        Ok(text) => (false, text.to_string()),
        Err(_) => (true, base64::engine::general_purpose::STANDARD.encode(body)),
    }
}
