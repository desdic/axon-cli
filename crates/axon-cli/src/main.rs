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
}

enum Output {
    Json(Value),
    /// Responses already written to stdout as they arrived.
    Written,
}

fn main() -> ExitCode {
    match run(Args::parse()) {
        Ok(Output::Json(output)) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&output).expect("JSON value serializes")
            );
            ExitCode::SUCCESS
        }
        Ok(Output::Written) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{}", json!({ "error": format!("{err:#}") }));
            ExitCode::FAILURE
        }
    }
}

fn run(args: Args) -> Result<Output> {
    let path = &args.file;
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let file = httpfile::parse(&contents)
        .with_context(|| format!("failed to parse {}", path.display()))?;

    if args.list {
        // An unnamed request is listed as the `#N` that selects it with -n.
        return Ok(Output::Json(
            file.requests
                .iter()
                .zip(1..)
                .map(|(r, index)| r.name.clone().unwrap_or(format!("#{index}")))
                .collect(),
        ));
    }
    if args.names.is_empty() {
        return Ok(Output::Json(
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
    for step in steps.iter_mut().filter(|step| !step.chained) {
        step.builder = Some(
            file.resolve(step.request, &no_responses, None)
                .and_then(|request| build(&client, request, netrc.as_deref()))
                .with_context(|| format!("request \"{}\"", step.label))?,
        );
    }

    let mut responses = HashMap::new();
    let mut json_output = Vec::new();
    let mut stdout = std::io::stdout().lock();
    let mut last_byte = None;
    for mut step in steps {
        let label = format!("request \"{}\"", step.label);
        // A looping request runs once per element; any other request once.
        let elements = match file
            .loop_items(step.request, &responses)
            .with_context(|| label.clone())?
        {
            Some(items) => items.into_iter().map(Some).collect(),
            None => vec![None],
        };
        let looping = step.request.foreach.is_some();
        let mut iterations = Vec::new();
        for (i, element) in elements.iter().enumerate() {
            let label = || {
                if looping {
                    format!("{label} (element {i})")
                } else {
                    label.clone()
                }
            };
            let builder = match step.builder.take() {
                Some(builder) => builder,
                None => file
                    .resolve(step.request, &responses, element.as_ref())
                    .and_then(|request| build(&client, request, netrc.as_deref()))
                    .with_context(label)?,
            };
            let received = builder
                .send()
                .map_err(anyhow::Error::from)
                .and_then(Received::read)
                .with_context(label)?;
            // Responses of a looping request cannot be referenced.
            if let Some(name) = step.request.name.as_ref().filter(|_| !looping) {
                responses.insert(name.clone(), received.captured());
            }

            if args.json {
                if step.selected || args.with_deps {
                    let mut output = received.json(step.index, step.request.name.as_deref());
                    if !step.selected {
                        output["dependency"] = json!(true);
                    }
                    iterations.push(output);
                }
                continue;
            }
            // -b prints only the -n requests; the default output prints every response.
            if args.body && !step.selected {
                continue;
            }
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
        // A looping -n takes one JSON slot holding all of its iterations.
        if args.json && (step.selected || args.with_deps) {
            if looping {
                json_output.push(Value::Array(iterations));
            } else {
                json_output.extend(iterations);
            }
        }
    }

    if !args.json {
        return Ok(Output::Written);
    }
    Ok(Output::Json(if args.names.len() == 1 && !args.with_deps {
        json_output.remove(0)
    } else {
        Value::Array(json_output)
    }))
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

        let references = self.file.references(request)?;
        self.stack.push(index);
        for reference in &references {
            let name = &reference.name;
            let (dep_index, dependency) = find_named(&self.file.requests, name)?
                .with_context(|| format!("no request named \"{name}\""))?;
            if dep_index == index {
                bail!("request \"{name}\" depends on itself");
            }
            if dependency.foreach.is_some() {
                bail!("request \"{name}\" loops (@foreach), so its responses cannot be referenced");
            }
            self.visit(dep_index, dependency, name, false)
                .with_context(|| format!("request \"{name}\""))?;
        }
        self.stack.pop();

        self.steps.push(Step {
            index,
            request,
            label: label.to_string(),
            selected,
            chained: !references.is_empty(),
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
