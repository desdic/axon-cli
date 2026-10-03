use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result, bail};
use axon_httpfile as httpfile;
use axon_netrc as netrc;
use base64::Engine;
use clap::Parser;
use reqwest::Method;
use reqwest::Url;
use reqwest::blocking::{Client, RequestBuilder, Response};
use reqwest::cookie::Cookie;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};
use reqwest::redirect::Policy;
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
}

enum Output {
    Json(Value),
    Raw(Vec<u8>),
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
        Ok(Output::Raw(body)) => {
            let mut stdout = std::io::stdout().lock();
            match stdout.write_all(&body).and_then(|()| stdout.flush()) {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!(
                        "{}",
                        json!({ "error": format!("failed to write body: {err}") })
                    );
                    ExitCode::FAILURE
                }
            }
        }
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

    // Build every selected request before sending any, so bad input fails early.
    let requests = args
        .names
        .iter()
        .map(|selector| {
            let (index, request) = select(&file.requests, selector)?;
            let name = request.name.clone();
            file.resolve(request)
                .and_then(|request| build(&client, request, netrc.as_deref()))
                .map(|builder| (selector, index, name, builder))
                .with_context(|| format!("request \"{selector}\""))
        })
        .collect::<Result<Vec<_>>>()?;

    let responses = requests
        .into_iter()
        .map(|(selector, index, name, request)| {
            request
                .send()
                .with_context(|| format!("request \"{selector}\""))
                .map(|response| (selector, index, name, response))
        });

    if !args.json {
        let mut output = Vec::new();
        for response in responses {
            let (selector, _, _, response) = response?;
            // Start each later header block on its own line; bare bodies stay as sent.
            if !args.body && output.last().is_some_and(|&byte| byte != b'\n') {
                output.push(b'\n');
            }
            render_raw(&mut output, response, !args.body)
                .with_context(|| format!("request \"{selector}\""))?;
        }
        return Ok(Output::Raw(output));
    }

    let mut responses = responses
        .map(|response| {
            response.and_then(|(_, index, name, response)| render(index, name.as_deref(), response))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Output::Json(if responses.len() == 1 {
        responses.remove(0)
    } else {
        Value::Array(responses)
    }))
}

/// Finds the one request with this exact name or, failing that, the request
/// at a 1-based `#N` index, returning it with its index.
fn select<'a>(
    requests: &'a [httpfile::Request],
    selector: &str,
) -> Result<(usize, &'a httpfile::Request)> {
    let mut matches = (1..)
        .zip(requests)
        .filter(|(_, r)| r.name.as_deref() == Some(selector));
    if let Some(found) = matches.next() {
        if matches.next().is_some() {
            bail!("more than one request is named \"{selector}\"");
        }
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

fn render(index: usize, name: Option<&str>, response: Response) -> Result<Value> {
    let status = response.status();
    let url = response.url().to_string();
    let version = format!("{:?}", response.version());
    let headers = headers_json(response.headers());
    let cookies: Vec<Value> = response.cookies().map(cookie_json).collect();
    let body = response.bytes().context("failed to read response body")?;
    let (base64, body) = body_text(&body);
    let mut output = json!({
        "index": index,
        "name": name,
        "url": url,
        "version": version,
        "status": { "code": status.as_u16(), "reason": status.canonical_reason() },
        "headers": headers,
        "cookies": cookies,
        "body": body,
    });
    if base64 {
        output["body_base64"] = json!(true);
    }
    Ok(output)
}

/// Writes the status line and headers like `curl -v`, each prefixed with `< `
/// (unless `head` is false), then the body exactly as received.
fn render_raw(output: &mut Vec<u8>, response: Response, head: bool) -> Result<()> {
    if head {
        output.extend_from_slice(raw_head(&response).as_bytes());
    }
    output.extend_from_slice(&response.bytes().context("failed to read response body")?);
    Ok(())
}

fn raw_head(response: &Response) -> String {
    let status = response.status();
    let mut head = format!("< {:?} {}", response.version(), status.as_u16());
    if let Some(reason) = status.canonical_reason() {
        head.push_str(&format!(" {reason}"));
    }
    head.push('\n');
    for (name, value) in response.headers() {
        head.push_str(&format!(
            "< {name}: {}\n",
            String::from_utf8_lossy(value.as_bytes())
        ));
    }
    head.push_str("< \n");
    head
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
