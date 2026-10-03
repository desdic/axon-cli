//! End-to-end tests: run the built binary against a local HTTP server.
//!
//! Every run sets `HOME` to a test-controlled directory, so the user's real
//! `~/.netrc` is never read.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;

use base64::Engine;
use serde_json::{Value, json};

// --- test server -----------------------------------------------------------

/// Starts a server on a random port and returns its base URL.
///
/// - `/redirect` → 302 to `/json`
/// - `/text`     → plain-text body
/// - `/bin`      → non-UTF-8 body
/// - `/missing`  → 404
/// - anything else → JSON echo of the request (method, authorization, cookie,
///   user-agent, x-test, body)
///
/// Every response sets two cookies.
fn start_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            thread::spawn(move || handle(stream));
        }
    });
    format!("http://{addr}")
}

fn handle(stream: TcpStream) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let mut request_line = line.split_whitespace();
    let method = request_line.next().unwrap_or_default().to_string();
    let path = request_line.next().unwrap_or_default().to_string();

    let mut headers = Vec::new();
    loop {
        line.clear();
        reader.read_line(&mut line).unwrap();
        let Some((name, value)) = line.trim_end().split_once(':') else {
            break;
        };
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
    }
    let header_values = |name: &str| -> Vec<String> {
        headers
            .iter()
            .filter(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
            .collect()
    };

    let length = header_values("content-length")
        .first()
        .map_or(0, |v| v.parse().unwrap());
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();

    let (status, extra_header, content_type, payload) = match path.as_str() {
        "/redirect" => (
            "302 Found",
            Some("Location: /json"),
            "text/plain",
            Vec::new(),
        ),
        "/text" => ("200 OK", None, "text/plain", b"hello text".to_vec()),
        "/bin" => (
            "200 OK",
            None,
            "application/octet-stream",
            vec![0x00, 0xff, 0xfe],
        ),
        "/raw" => (
            "200 OK",
            None,
            "application/json",
            b"{ \"b\": 1,\n  \"a\": [1.0, 2e3] }\n".to_vec(),
        ),
        "/missing" => ("404 Not Found", None, "text/plain", b"not found".to_vec()),
        _ => {
            let echo = json!({
                "method": method,
                "authorization": header_values("authorization").first(),
                "cookie": header_values("cookie").first(),
                "user_agent": header_values("user-agent").first(),
                "x_test": header_values("x-test"),
                "body": String::from_utf8_lossy(&body),
            });
            (
                "200 OK",
                None,
                "application/json",
                echo.to_string().into_bytes(),
            )
        }
    };

    let mut response = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         Set-Cookie: session=abc123; Path=/; HttpOnly; SameSite=Lax; Max-Age=3600\r\n\
         Set-Cookie: theme=dark; Secure; SameSite=Strict\r\n",
        payload.len()
    );
    if let Some(header) = extra_header {
        response.push_str(header);
        response.push_str("\r\n");
    }
    response.push_str("\r\n");

    let mut stream = reader.into_inner();
    stream.write_all(response.as_bytes()).unwrap();
    if method != "HEAD" {
        stream.write_all(&payload).unwrap();
    }
}

// --- running the binary ----------------------------------------------------

struct Run {
    code: i32,
    stdout: Value,
    stderr: Value,
}

impl Run {
    /// A field of the JSON echo returned by the test server.
    fn echo(&self, field: &str) -> Value {
        echo_field(&self.stdout, field)
    }
}

/// A field of the JSON echo in one response's raw body.
fn echo_field(response: &Value, field: &str) -> Value {
    let body: Value = serde_json::from_str(response["body"].as_str().unwrap()).unwrap();
    body[field].clone()
}

/// Runs the binary with `--json`, so responses print as JSON.
fn run_in(home: &Path, args: &[&str], envs: &[(&str, &str)]) -> Run {
    let output = Command::new(env!("CARGO_BIN_EXE_axon-cli"))
        .arg("--json")
        .args(args)
        .env("HOME", home)
        .envs(envs.iter().copied())
        .output()
        .unwrap();

    let parse = |bytes: &[u8], stream: &str| -> Value {
        if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(bytes).unwrap_or_else(|e| {
                panic!(
                    "{stream} is not JSON ({e}): {}",
                    String::from_utf8_lossy(bytes)
                )
            })
        }
    };
    Run {
        code: output.status.code().expect("process exited normally"),
        stdout: parse(&output.stdout, "stdout"),
        stderr: parse(&output.stderr, "stderr"),
    }
}

/// A directory under Cargo's per-target temp dir, unique per `name`.
fn temp_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Writes `contents` to a `.http` file unique to `test` and returns its path.
fn http_file(test: &str, contents: &str) -> String {
    let path = temp_dir(test).join("requests.http");
    std::fs::write(&path, contents).unwrap();
    path.to_str().unwrap().to_string()
}

/// Runs `-f <file with contents> args...` with a `HOME` that has no `.netrc`.
fn run_file(test: &str, contents: &str, args: &[&str]) -> Run {
    let file = http_file(test, contents);
    let mut all = vec!["-f", file.as_str()];
    all.extend(args);
    run_in(&temp_dir("empty-home"), &all, &[])
}

/// Runs a file holding just `request`, selected by name.
fn run_request(test: &str, request: &str) -> Run {
    run_file(test, &format!("### r\n{request}\n"), &["-n", "r"])
}

/// A `HOME` directory containing `.netrc` with the given contents.
fn home_with_netrc(name: &str, netrc: &str) -> PathBuf {
    let home = temp_dir(name);
    std::fs::write(home.join(".netrc"), netrc).unwrap();
    home
}

/// Runs `request` with a `HOME` whose `.netrc` has the given contents.
fn run_with_netrc(test: &str, netrc: &str, request: &str) -> Run {
    let home = home_with_netrc(test, netrc);
    let file = http_file(test, &format!("### r\n{request}\n"));
    run_in(&home, &["-f", &file, "-n", "r"], &[])
}

fn basic(login: &str, password: &str) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{login}:{password}"));
    format!("Basic {encoded}")
}

// --- listing ---------------------------------------------------------------

#[test]
fn lists_requests_without_name() {
    let r = run_file(
        "list",
        "@host = example.com\n### first\nGET https://{{host}}/a\n\n### ignored\n# @name second\npost https://{{host}}/b\n\nbody\n",
        &[],
    );

    assert_eq!(r.code, 0);
    assert_eq!(
        r.stdout,
        json!([
            { "index": 1, "name": "first", "method": "GET", "url": "https://{{host}}/a", "start_line": 2, "end_line": 4 },
            { "index": 2, "name": "second", "method": "POST", "url": "https://{{host}}/b", "start_line": 5, "end_line": 9 },
        ])
    );
}

#[test]
fn list_option_prints_only_names() {
    let r = run_file(
        "list-names",
        "GET http://x/unnamed\n### first\nGET http://x/1\n\n### ignored\n# @name create item\nPOST http://x/2\n",
        &["--list"],
    );

    assert_eq!(r.code, 0);
    assert_eq!(r.stdout, json!(["#1", "first", "create item"]));
}

#[test]
fn list_option_conflicts_with_name() {
    let file = http_file("list-conflict", "### a\nGET http://x/\n");
    let output = Command::new(env!("CARGO_BIN_EXE_axon-cli"))
        .args(["-f", &file, "-l", "-n", "a"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot be used with"));
}

// --- response output -------------------------------------------------------

#[test]
fn json_response_shape() {
    let base = start_server();
    let url = format!("{base}/json");
    let r = run_request("shape", &format!("GET {url}"));

    assert_eq!(r.code, 0);
    assert_eq!(r.stderr, Value::Null);
    assert_eq!(r.stdout["index"], 1);
    assert_eq!(r.stdout["name"], "r");
    assert_eq!(r.stdout["url"], url);
    assert_eq!(r.stdout["version"], "HTTP/1.1");
    assert_eq!(r.stdout["status"], json!({ "code": 200, "reason": "OK" }));
    assert_eq!(
        r.stdout["headers"]["content-type"],
        json!(["application/json"])
    );
    assert_eq!(r.stdout["body_base64"], Value::Null);
    assert_eq!(r.echo("method"), "GET");
    assert_eq!(r.echo("authorization"), Value::Null);
    assert_eq!(
        r.echo("user_agent"),
        format!("axon-cli/{}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn repeated_response_headers_are_preserved() {
    let base = start_server();
    let r = run_request("resp-headers", &format!("GET {base}/json"));

    assert_eq!(
        r.stdout["headers"]["set-cookie"],
        json!([
            "session=abc123; Path=/; HttpOnly; SameSite=Lax; Max-Age=3600",
            "theme=dark; Secure; SameSite=Strict",
        ])
    );
}

#[test]
fn cookies_are_parsed() {
    let base = start_server();
    let r = run_request("cookies", &format!("GET {base}/json"));

    assert_eq!(
        r.stdout["cookies"],
        json!([
            {
                "name": "session", "value": "abc123", "domain": null, "path": "/",
                "expires": null, "max_age": 3600, "secure": false, "http_only": true, "same_site": "Lax",
            },
            {
                "name": "theme", "value": "dark", "domain": null, "path": null,
                "expires": null, "max_age": null, "secure": true, "http_only": false, "same_site": "Strict",
            },
        ])
    );
}

#[test]
fn text_body() {
    let base = start_server();
    let r = run_request("text", &format!("GET {base}/text"));

    assert_eq!(r.stdout["body_base64"], Value::Null);
    assert_eq!(r.stdout["body"], "hello text");
}

#[test]
fn json_body_is_raw_and_unreformatted() {
    let base = start_server();
    let r = run_request("raw", &format!("GET {base}/raw"));

    assert_eq!(r.stdout["body_base64"], Value::Null);
    assert_eq!(r.stdout["body"], "{ \"b\": 1,\n  \"a\": [1.0, 2e3] }\n");
}

#[test]
fn binary_body_is_base64() {
    let base = start_server();
    let r = run_request("bin", &format!("GET {base}/bin"));

    assert_eq!(r.stdout["body_base64"], true);
    assert_eq!(r.stdout["body"], "AP/+");
}

#[test]
fn head_has_empty_body() {
    let base = start_server();
    let r = run_request("head", &format!("HEAD {base}/json"));

    assert_eq!(r.code, 0);
    assert_eq!(r.stdout["status"]["code"], 200);
    assert_eq!(r.stdout["body_base64"], Value::Null);
    assert_eq!(r.stdout["body"], "");
}

#[test]
fn http_error_status_exits_zero() {
    let base = start_server();
    let r = run_request("404", &format!("GET {base}/missing"));

    assert_eq!(r.code, 0);
    assert_eq!(
        r.stdout["status"],
        json!({ "code": 404, "reason": "Not Found" })
    );
    assert_eq!(r.stdout["body"], "not found");
}

// --- default output: headers and raw body ----------------------------------

/// Runs `-f <file with contents> args...` without `--json` and returns (exit code, raw stdout).
fn run_body(test: &str, contents: &str, args: &[&str]) -> (i32, Vec<u8>) {
    let file = http_file(test, contents);
    let output = Command::new(env!("CARGO_BIN_EXE_axon-cli"))
        .args(["-f", file.as_str()])
        .args(args)
        .env("HOME", temp_dir("empty-home"))
        .output()
        .unwrap();
    (
        output.status.code().expect("process exited normally"),
        output.stdout,
    )
}

/// The `< ` header block the test server sends with every response.
fn raw_head(status: &str, content_type: &str, length: usize) -> String {
    format!(
        "< HTTP/1.1 {status}\n\
         < content-type: {content_type}\n\
         < content-length: {length}\n\
         < connection: close\n\
         < set-cookie: session=abc123; Path=/; HttpOnly; SameSite=Lax; Max-Age=3600\n\
         < set-cookie: theme=dark; Secure; SameSite=Strict\n\
         < \n"
    )
}

/// Splits raw output after the `< ` header block, returning the body.
fn raw_body(stdout: &[u8]) -> &[u8] {
    let end = stdout
        .windows(3)
        .position(|w| w == b"< \n")
        .expect("header block ends with \"< \"");
    &stdout[end + 3..]
}

#[test]
fn prints_headers_and_raw_body_by_default() {
    let base = start_server();
    let (code, stdout) = run_body(
        "body-text",
        &format!("### r\nGET {base}/text\n"),
        &["-n", "r"],
    );

    assert_eq!(code, 0);
    assert_eq!(
        String::from_utf8(stdout).unwrap(),
        raw_head("200 OK", "text/plain", 10) + "hello text"
    );
}

#[test]
fn raw_json_body_is_unreformatted() {
    let base = start_server();
    let (code, stdout) = run_body(
        "body-raw",
        &format!("### r\nGET {base}/raw\n"),
        &["-n", "r"],
    );

    assert_eq!(code, 0);
    assert_eq!(raw_body(&stdout), b"{ \"b\": 1,\n  \"a\": [1.0, 2e3] }\n");
}

#[test]
fn raw_binary_body_is_written_as_is() {
    let base = start_server();
    let (code, stdout) = run_body(
        "body-bin",
        &format!("### r\nGET {base}/bin\n"),
        &["-n", "r"],
    );

    assert_eq!(code, 0);
    assert_eq!(raw_body(&stdout), [0x00, 0xff, 0xfe]);
}

#[test]
fn several_raw_responses_print_in_order_on_separate_lines() {
    let base = start_server();
    let (code, stdout) = run_body(
        "body-several",
        &format!("### text\nGET {base}/text\n\n### bin\nGET {base}/bin\n"),
        &["-n", "bin", "-n", "text"],
    );

    let mut expected = raw_head("200 OK", "application/octet-stream", 3).into_bytes();
    expected.extend_from_slice(b"\x00\xff\xfe\n");
    expected.extend_from_slice(raw_head("200 OK", "text/plain", 10).as_bytes());
    expected.extend_from_slice(b"hello text");
    assert_eq!(code, 0);
    assert_eq!(stdout, expected);
}

#[test]
fn body_option_prints_only_raw_body() {
    let base = start_server();
    let (code, stdout) = run_body(
        "body-only",
        &format!("### r\nGET {base}/raw\n"),
        &["-n", "r", "-b"],
    );

    assert_eq!(code, 0);
    assert_eq!(stdout, b"{ \"b\": 1,\n  \"a\": [1.0, 2e3] }\n");
}

#[test]
fn body_option_concatenates_several_bodies_as_sent() {
    let base = start_server();
    let (code, stdout) = run_body(
        "body-only-several",
        &format!("### text\nGET {base}/text\n\n### bin\nGET {base}/bin\n"),
        &["-n", "bin", "-n", "text", "-b"],
    );

    assert_eq!(code, 0);
    assert_eq!(stdout, b"\x00\xff\xfehello text");
}

#[test]
fn body_option_conflicts_with_json() {
    let (code, stdout) = run_body(
        "body-json",
        "### a\nGET http://x/\n",
        &["-n", "a", "-b", "--json"],
    );

    assert_eq!(code, 2);
    assert!(stdout.is_empty());
}

#[test]
fn listing_is_json_without_json_option() {
    let (code, stdout) = run_body("raw-list", "### a\nGET http://x/\n", &["-l"]);

    assert_eq!(code, 0);
    assert_eq!(
        serde_json::from_slice::<Value>(&stdout).unwrap(),
        json!(["a"])
    );
}

// --- redirects -------------------------------------------------------------

#[test]
fn redirects_are_followed() {
    let base = start_server();
    let r = run_request("redirect", &format!("GET {base}/redirect"));

    assert_eq!(r.stdout["url"], format!("{base}/json"));
    assert_eq!(r.stdout["status"]["code"], 200);
}

#[test]
fn no_follow_returns_redirect() {
    let base = start_server();
    let r = run_file(
        "no-follow",
        &format!("### r\nGET {base}/redirect\n"),
        &["-n", "r", "--no-follow"],
    );

    assert_eq!(r.stdout["url"], format!("{base}/redirect"));
    assert_eq!(
        r.stdout["status"],
        json!({ "code": 302, "reason": "Found" })
    );
    assert_eq!(r.stdout["headers"]["location"], json!(["/json"]));
}

// --- request construction --------------------------------------------------

#[test]
fn headers_and_body_are_sent() {
    let base = start_server();
    let r = run_request(
        "headers-body",
        &format!("POST {base}/json\nX-Test: a\nX-Test: b\n\n{{\n  \"k\": 1\n}}\n"),
    );

    assert_eq!(r.echo("method"), "POST");
    assert_eq!(r.echo("x_test"), json!(["a", "b"]));
    assert_eq!(r.echo("body"), "{\n  \"k\": 1\n}");
}

#[test]
fn url_only_defaults_to_get() {
    let base = start_server();
    let r = run_request("url-only", &format!("{base}/json"));

    assert_eq!(r.echo("method"), "GET");
}

#[test]
fn method_is_case_insensitive_and_version_is_accepted() {
    let base = start_server();
    let r = run_request("method-case", &format!("put {base}/json HTTP/1.1"));

    assert_eq!(r.echo("method"), "PUT");
}

#[test]
fn request_can_set_user_agent() {
    let base = start_server();
    let r = run_request(
        "user-agent",
        &format!("GET {base}/json\nUser-Agent: custom/1"),
    );

    assert_eq!(r.echo("user_agent"), "custom/1");
}

// --- variables -------------------------------------------------------------

#[test]
fn file_and_env_variables() {
    let base = start_server();
    let file = http_file(
        "variables",
        &format!(
            "@base = {base}\n@url = {{{{base}}}}/json\n### r\nPOST {{{{url}}}}\nX-Test: {{{{$env AXON_CLI_TEST_VAR}}}}\n\nurl={{{{ url }}}}\n"
        ),
    );
    let r = run_in(
        &temp_dir("empty-home"),
        &["-f", &file, "-n", "r"],
        &[("AXON_CLI_TEST_VAR", "from env")],
    );

    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(r.stdout["url"], format!("{base}/json"));
    assert_eq!(r.echo("x_test"), json!(["from env"]));
    assert_eq!(r.echo("body"), format!("url={base}/json"));
}

// --- selecting requests ----------------------------------------------------

#[test]
fn several_names_run_in_order_given() {
    let base = start_server();
    let r = run_file(
        "several",
        &format!("### get\nGET {base}/json\n\n### create item\nPOST {base}/json\n\nnew\n"),
        &["-n", "create item", "-n", "get"],
    );

    assert_eq!(r.code, 0);
    assert_eq!(r.stdout[0]["name"], "create item");
    assert_eq!(echo_field(&r.stdout[0], "method"), "POST");
    assert_eq!(r.stdout[1]["name"], "get");
    assert_eq!(echo_field(&r.stdout[1], "method"), "GET");
}

#[test]
fn name_directive_selects_request() {
    let base = start_server();
    let r = run_file(
        "name-directive",
        &format!("### Some title\n# @name real\nGET {base}/json\n"),
        &["-n", "real"],
    );

    assert_eq!(r.code, 0);
    assert_eq!(r.stdout["name"], "real");
}

#[test]
fn index_selects_request_named_or_not() {
    let base = start_server();
    let r = run_file(
        "index",
        &format!(
            "@x = 1\n###\n// only a comment\nGET {base}/json\n\n### create\nPOST {base}/json\n"
        ),
        &["-n", "#2", "-n", "#1", "-n", "create"],
    );

    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(r.stdout[0]["index"], 2);
    assert_eq!(r.stdout[0]["name"], "create");
    assert_eq!(echo_field(&r.stdout[0], "method"), "POST");
    assert_eq!(r.stdout[1]["index"], 1);
    assert_eq!(r.stdout[1]["name"], Value::Null);
    assert_eq!(echo_field(&r.stdout[1], "method"), "GET");
    assert_eq!(r.stdout[2]["index"], 2);
}

#[test]
fn exact_name_wins_over_index() {
    let base = start_server();
    let r = run_file(
        "index-name",
        &format!("### a\nGET {base}/json\n\n### #1\nPOST {base}/json\n"),
        &["-n", "#1"],
    );

    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(r.stdout["index"], 2);
    assert_eq!(echo_field(&r.stdout, "method"), "POST");
}

#[test]
fn cookies_are_shared_between_requests() {
    let base = start_server();
    let r = run_file(
        "shared-cookies",
        &format!("### first\nGET {base}/json\n\n### second\nGET {base}/json\n"),
        &["-n", "first", "-n", "second"],
    );

    assert_eq!(echo_field(&r.stdout[0], "cookie"), Value::Null);
    // `theme` is Secure, but loopback counts as a secure context.
    // The cookie store sends cookies in no fixed order.
    let cookie = echo_field(&r.stdout[1], "cookie");
    let mut sent: Vec<_> = cookie.as_str().unwrap().split("; ").collect();
    sent.sort();
    assert_eq!(sent, ["session=abc123", "theme=dark"]);
}

// --- netrc -----------------------------------------------------------------

#[test]
fn netrc_from_default_home_location() {
    let base = start_server();
    let r = run_with_netrc(
        "netrc-home",
        "machine 127.0.0.1 login alice password s3cret\n",
        &format!("GET {base}/json"),
    );

    assert_eq!(r.echo("authorization"), basic("alice", "s3cret"));
}

#[test]
fn netrc_file_option() {
    let base = start_server();
    let netrc = temp_dir("netrc-option").join("custom-netrc");
    std::fs::write(&netrc, "machine 127.0.0.1 login carol password pw\n").unwrap();
    let r = run_file(
        "netrc-option",
        &format!("### r\nGET {base}/json\n"),
        &["-n", "r", "--netrc-file", netrc.to_str().unwrap()],
    );

    assert_eq!(r.echo("authorization"), basic("carol", "pw"));
}

#[test]
fn netrc_default_entry_is_fallback() {
    let base = start_server();
    let r = run_with_netrc(
        "netrc-default",
        "machine other.example login x password y\ndefault login anon password guest\n",
        &format!("GET {base}/json"),
    );

    assert_eq!(r.echo("authorization"), basic("anon", "guest"));
}

#[test]
fn netrc_without_matching_machine_sends_no_auth() {
    let base = start_server();
    let r = run_with_netrc(
        "netrc-no-match",
        "machine other.example login x password y\n",
        &format!("GET {base}/json"),
    );

    assert_eq!(r.echo("authorization"), Value::Null);
}

#[test]
fn authorization_header_overrides_netrc() {
    let base = start_server();
    let r = run_with_netrc(
        "netrc-vs-header",
        "machine 127.0.0.1 login alice password s3cret\n",
        &format!("GET {base}/json\nAuthorization: Bearer token"),
    );

    assert_eq!(r.echo("authorization"), "Bearer token");
}

#[test]
fn url_credentials_override_netrc() {
    let base = start_server();
    let url = format!("{}/json", base.replace("http://", "http://bob:pw@"));
    let r = run_with_netrc(
        "netrc-vs-url",
        "machine 127.0.0.1 login alice password s3cret\n",
        &format!("GET {url}"),
    );

    assert_eq!(r.echo("authorization"), basic("bob", "pw"));
}

// --- errors ----------------------------------------------------------------

fn assert_error(r: &Run, message_contains: &str) {
    assert_eq!(r.code, 1);
    assert_eq!(r.stdout, Value::Null);
    let message = r.stderr["error"]
        .as_str()
        .expect("stderr has an error string");
    assert!(
        message.contains(message_contains),
        "unexpected error: {message}"
    );
}

fn refused_url() -> String {
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    format!("http://127.0.0.1:{port}/json")
}

#[test]
fn connection_refused() {
    let r = run_request("refused", &format!("GET {}", refused_url()));

    assert_error(&r, "error sending request");
}

#[test]
fn unknown_name_is_reported_before_sending() {
    // `a` would fail to connect; the unknown name must be reported instead.
    let r = run_file(
        "unknown-name",
        &format!("### a\nGET {}\n", refused_url()),
        &["-n", "a", "-n", "nope"],
    );

    assert_error(&r, "no request named \"nope\"");
}

#[test]
fn index_out_of_range() {
    for index in ["#0", "#3", "#99999999999999999999999"] {
        let r = run_file(
            "index-range",
            "### a\nGET http://x/1\n\n### b\nGET http://x/2\n",
            &["-n", index],
        );

        assert_error(&r, &format!("no request {index} (the file has 2 requests)"));
    }
}

#[test]
fn malformed_index_is_a_name() {
    for selector in ["#", "#+1", "#1a", "1"] {
        let r = run_file(
            "index-malformed",
            "### a\nGET http://x/1\n",
            &["-n", selector],
        );

        assert_error(&r, &format!("no request named \"{selector}\""));
    }
}

#[test]
fn duplicate_name() {
    let r = run_file(
        "duplicate-name",
        "### a\nGET http://x/1\n\n### a\nGET http://x/2\n",
        &["-n", "a"],
    );

    assert_error(&r, "more than one request is named \"a\"");
}

#[test]
fn names_are_case_sensitive() {
    let r = run_file("name-case", "### Get\nGET http://x/1\n", &["-n", "get"]);

    assert_error(&r, "no request named \"get\"");
}

#[test]
fn undefined_variable() {
    let r = run_request("undefined-var", "GET http://{{host}}/x");

    assert_error(&r, "undefined variable: host");
}

#[test]
fn missing_http_file() {
    let r = run_in(
        &temp_dir("empty-home"),
        &["-f", "/nonexistent/requests.http"],
        &[],
    );

    assert_error(&r, "failed to read /nonexistent/requests.http");
}

#[test]
fn parse_error_reports_line() {
    let r = run_request("parse-error", "GET http://x\nno-colon");

    assert_error(&r, "line 3: invalid header");
}

#[test]
fn missing_netrc_file_option() {
    let base = start_server();
    let r = run_file(
        "missing-netrc",
        &format!("### r\nGET {base}/json\n"),
        &["-n", "r", "--netrc-file", "/nonexistent/netrc"],
    );

    assert_error(&r, "failed to read netrc file");
}

#[test]
fn invalid_url() {
    let r = run_request("invalid-url", "GET not-a-url");

    assert_error(&r, "invalid URL");
}

#[test]
fn invalid_method() {
    let r = run_request("invalid-method", "G(T http://x/");

    assert_error(&r, "invalid method");
}
