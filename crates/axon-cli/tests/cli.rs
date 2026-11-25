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
/// - `/list`     → JSON with arrays to loop over
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
        "/list" => (
            "200 OK",
            None,
            "application/json",
            br#"{"users":[{"id":1,"name":"ann"},{"id":2,"name":"bob"}],"empty":[],"one":"x"}"#
                .to_vec(),
        ),
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
    let file = temp_dir("list").join("requests.http");
    let file = file.to_str().unwrap();
    assert_eq!(
        r.stdout,
        json!([
            { "index": 1, "name": "first", "file": file, "method": "GET", "url": "https://{{host}}/a", "start_line": 2, "end_line": 4 },
            { "index": 2, "name": "second", "file": file, "method": "POST", "url": "https://{{host}}/b", "start_line": 5, "end_line": 9 },
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

#[test]
fn var_option_replaces_file_variables() {
    let base = start_server();
    let contents = format!(
        "@host = nowhere.invalid\n@url = {{{{host}}}}/json\n### r\nGET {base}/{{{{url}}}}?u={{{{user}}}}\n"
    );
    let r = run_file(
        "var-option",
        &contents,
        &["-n", "r", "--var", "host=x", "--var", "user=a=b"],
    );

    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(r.stdout["url"], format!("{base}/x/json?u=a=b"));

    let r = run_file("var-option-bad", &contents, &["-n", "r", "--var", "a.b=1"]);
    assert_eq!(r.code, 1);
    assert_eq!(
        r.stderr["error"],
        "--var: invalid variable name (use letters, digits, _ and -): a.b"
    );
}

// --- delays ----------------------------------------------------------------

/// Runs `run` and returns how long it took.
fn timed(run: impl FnOnce() -> Run) -> (Run, std::time::Duration) {
    let start = std::time::Instant::now();
    let r = run();
    (r, start.elapsed())
}

#[test]
fn delay_option_waits_between_requests() {
    let base = start_server();
    // `list` runs as a dependency, then `each` twice: two waits.
    let (r, elapsed) = timed(|| {
        run_file(
            "delay-option",
            &loops(&base),
            &["-n", "each", "--delay", "300ms"],
        )
    });
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(elapsed.as_millis() >= 600, "{elapsed:?}");
}

#[test]
fn delay_directive_waits_after_each_iteration_but_not_the_last_request() {
    let base = start_server();
    let contents = format!(
        "### list\nGET {base}/list\n\n### each\n# @foreach user in {{{{list.response.body.$.users}}}}\n# @delay 1s\nGET {base}/json\n"
    );
    let (r, elapsed) = timed(|| run_file("delay-directive", &contents, &["-n", "each"]));
    assert_eq!(r.code, 0, "{}", r.stderr);
    // Between the two iterations only, not after the second.
    assert!((1000..1900).contains(&elapsed.as_millis()), "{elapsed:?}");
}

#[test]
fn delay_directive_replaces_delay_option() {
    let base = start_server();
    let contents = format!("### a\n# @delay 0\nGET {base}/json\n\n### b\nGET {base}/json\n");
    let (r, elapsed) = timed(|| {
        run_file(
            "delay-zero",
            &contents,
            &["-n", "a", "-n", "b", "--delay", "5s"],
        )
    });
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(elapsed.as_millis() < 3000, "{elapsed:?}");
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

// --- response chaining -----------------------------------------------------

/// `login` posts a token that the echo server returns in its body; `me` and
/// `again` reference that response, directly and through a file variable.
/// `BASE` stands for the server URL.
const CHAIN: &str = "\
@token = {{login.response.body.$.body}}
### login
POST BASE/json

tok123

### me
GET BASE/json
Authorization: Bearer {{token}}
X-Test: {{ login.response.headers.Content-Type }}
X-Test: {{login.response.body.$.x_test}}

### again
GET BASE/json
X-Test: {{login.response.body.$.method}}
";

fn chain(base: &str) -> String {
    CHAIN.replace("BASE", base)
}

/// How many responses the default output printed.
fn response_count(stdout: &[u8]) -> usize {
    stdout.windows(11).filter(|w| w == b"< HTTP/1.1 ").count()
}

#[test]
fn chained_values_are_substituted() {
    let base = start_server();
    let r = run_file("chain-values", &chain(&base), &["-n", "me"]);

    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(r.echo("authorization"), "Bearer tok123");
    assert_eq!(r.echo("x_test"), json!(["application/json", "[]"]));
}

#[test]
fn chained_cookies_are_substituted() {
    let base = start_server();
    let r = run_file(
        "chain-cookies",
        &format!(
            "### login\nGET {base}/json\n\n### me\nGET {base}/json\nX-Test: {{{{login.response.cookies.session}}}}\nX-Test: {{{{ login.response.cookies.theme }}}}\n"
        ),
        &["-n", "me"],
    );

    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(r.echo("x_test"), json!(["abc123", "dark"]));
}

#[test]
fn chained_values_can_be_filtered() {
    let base = start_server();
    let r = run_file(
        "chain-filters",
        &format!(
            "### login\nGET {base}/json\n\n### me\nGET {base}/json\nX-Test: {{{{login.response.headers.Content-Type | split(\"/\", -1)}}}}\nX-Test: {{{{login.response.cookies.session | suffix(\"123\") | prefix(\"a\")}}}}\n"
        ),
        &["-n", "me"],
    );

    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(r.echo("x_test"), json!(["json", "bc"]));
}

#[test]
fn invalid_filter_is_reported_before_sending() {
    let r = run_file(
        "chain-bad-filter",
        &format!(
            "### login\nGET {}\n\n### me\nGET http://x/{{{{login.response.headers.X | splitt(\"/\", 0)}}}}\n",
            refused_url()
        ),
        &["-n", "me"],
    );

    assert_error(
        &r,
        "request \"me\": invalid filter in {{login.response.headers.X | splitt(\"/\", 0)}}: unknown filter \"splitt\" (expected split, trim, prefix or suffix)",
    );
}

#[test]
fn failing_filter_stops_the_run() {
    let base = start_server();
    let r = run_file(
        "chain-filter-fails",
        &format!(
            "### login\nGET {base}/json\n\n### me\nGET {base}/json\nX-Test: {{{{login.response.cookies.theme | split(\"/\", 1)}}}}\n"
        ),
        &["-n", "me"],
    );

    assert_error(
        &r,
        "request \"me\": {{login.response.cookies.theme | split(\"/\", 1)}}: split(\"/\", 1): \"dark\" gives 1 part",
    );
}

#[test]
fn json_includes_only_selected_requests() {
    let base = start_server();
    let one = run_file("chain-json-one", &chain(&base), &["-n", "me"]);
    assert_eq!(one.stdout["name"], "me");

    let two = run_file(
        "chain-json-two",
        &chain(&base),
        &["-n", "me", "-n", "again"],
    );
    assert_eq!(two.code, 0, "{}", two.stderr);
    assert_eq!(two.stdout.as_array().unwrap().len(), 2);
    assert_eq!(two.stdout[0]["name"], "me");
    assert_eq!(two.stdout[1]["name"], "again");
    assert_eq!(echo_field(&two.stdout[1], "x_test"), json!(["POST"]));
}

#[test]
fn with_deps_includes_dependencies_in_order() {
    let base = start_server();
    let r = run_file(
        "chain-with-deps",
        &chain(&base),
        &["-n", "me", "--with-deps"],
    );
    assert_eq!(r.code, 0, "{}", r.stderr);
    let all = r
        .stdout
        .as_array()
        .expect("--with-deps always prints an array");
    assert_eq!(all.len(), 2);
    assert_eq!(all[0]["name"], "login");
    assert_eq!(all[0]["dependency"], true);
    assert_eq!(all[1]["name"], "me");
    assert!(all[1].get("dependency").is_none());

    // A dependency that was already given with -n is not sent again.
    let r = run_file(
        "chain-with-deps-two",
        &chain(&base),
        &["-n", "login", "-n", "me", "--with-deps"],
    );
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(r.stdout.as_array().unwrap().len(), 2);
    assert!(r.stdout[0].get("dependency").is_none());
}

#[test]
fn with_deps_keeps_a_looping_request_in_one_slot() {
    let base = start_server();
    let r = run_file(
        "loop-with-deps",
        &loops(&base),
        &["-n", "each", "--with-deps"],
    );
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(r.stdout[0]["name"], "list");
    assert_eq!(r.stdout[0]["dependency"], true);
    assert_eq!(r.stdout[1].as_array().unwrap().len(), 2);
}

#[test]
fn default_output_includes_dependencies_run_once() {
    let base = start_server();
    for (args, expected) in [
        (&["-n", "me"][..], 2),
        (&["-n", "me", "-n", "again"][..], 3),
        (&["-n", "login", "-n", "me"][..], 2),
        // A request given with -n always runs, even after running as a dependency.
        (&["-n", "me", "-n", "login"][..], 3),
    ] {
        let (code, stdout) = run_body("chain-default", &chain(&base), args);

        assert_eq!(code, 0);
        assert_eq!(response_count(&stdout), expected, "{args:?}");
    }
}

#[test]
fn body_option_prints_only_selected_requests() {
    let base = start_server();
    let (code, stdout) = run_body("chain-body", &chain(&base), &["-n", "me", "-b"]);

    assert_eq!(code, 0);
    let body: Value = serde_json::from_slice(&stdout).unwrap();
    assert_eq!(body["authorization"], "Bearer tok123");
}

#[test]
fn dependency_cycle_is_reported_before_sending() {
    let url = refused_url();
    let r = run_file(
        "chain-cycle",
        &format!(
            "### a\nGET {url}?{{{{b.response.headers.X}}}}\n\n### b\nGET {url}?{{{{a.response.headers.X}}}}\n"
        ),
        &["-n", "a"],
    );

    assert_error(&r, "dependency cycle: a -> b -> a");
}

#[test]
fn self_reference_is_reported() {
    let r = run_request("chain-self", "GET http://x/{{r.response.headers.X}}");

    assert_error(&r, "request \"r\" depends on itself");
}

#[test]
fn unknown_dependency_is_reported_before_sending() {
    let r = run_file(
        "chain-unknown",
        &format!(
            "### a\nGET {}\n\n### me\nGET http://x/{{{{login.response.headers.X}}}}\n",
            refused_url()
        ),
        &["-n", "a", "-n", "me"],
    );

    assert_error(&r, "no request named \"login\"");
}

#[test]
fn invalid_reference_is_reported_before_sending() {
    let r = run_file(
        "chain-invalid",
        &format!(
            "### login\nGET {}\n\n### me\nGET http://x/{{{{login.response.body.token}}}}\n",
            refused_url()
        ),
        &["-n", "me"],
    );

    assert_error(
        &r,
        "invalid response reference {{login.response.body.token}}",
    );
}

#[test]
fn missing_value_is_a_runtime_error() {
    let base = start_server();
    let r = run_file(
        "chain-missing",
        &format!(
            "### login\nPOST {base}/json\n\n### me\nGET {base}/json\nX-Test: {{{{login.response.body.$.nope}}}}\n"
        ),
        &["-n", "me"],
    );

    assert_error(&r, "response body of \"login\" has no value at $.nope");
}

// --- loops -----------------------------------------------------------------

/// `each` runs once per user from `list`; the others cover an empty array, a
/// non-array, a missing element value, and a reference to a looping request.
/// `BASE` stands for the server URL.
const LOOPS: &str = "\
### list
GET BASE/list

### each
# @foreach user in {{list.response.body.$.users}}
GET BASE/json
X-Test: {{user.id}}-{{user.name}}
X-Test: {{user}}

### none
# @foreach user in {{list.response.body.$.empty}}
GET BASE/json

### not array
// @foreach user in {{list.response.body.$.one}}
GET BASE/json

### missing
# @foreach user in {{list.response.body.$.users}}
GET BASE/json
X-Test: {{user.nope}}

### uses each
GET BASE/json
X-Test: {{each.response.headers.content-type}}
";

fn loops(base: &str) -> String {
    LOOPS.replace("BASE", base)
}

#[test]
fn loop_runs_once_per_element() {
    let base = start_server();
    let r = run_file("loop-each", &loops(&base), &["-n", "each"]);

    assert_eq!(r.code, 0, "{}", r.stderr);
    let iterations = r.stdout.as_array().expect("a looping -n prints an array");
    assert_eq!(iterations.len(), 2);
    assert_eq!(
        echo_field(&iterations[0], "x_test"),
        json!(["1-ann", r#"{"id":1,"name":"ann"}"#])
    );
    assert_eq!(
        echo_field(&iterations[1], "x_test"),
        json!(["2-bob", r#"{"id":2,"name":"bob"}"#])
    );
}

#[test]
fn loop_over_file_lines_and_json() {
    let base = start_server();
    let dir = temp_dir("loop-file");
    std::fs::write(dir.join("ids.txt"), "# ids\n7\n\n8\n").unwrap();
    std::fs::write(
        dir.join("users.json"),
        r#"[{"name": "ann"}, {"name": "bob"}]"#,
    )
    .unwrap();
    // Relative to the .http file, which http_file writes into the same dir.
    let contents = format!(
        "### ids\n# @foreach id in {{{{$file ids.txt}}}}\nGET {base}/json\nX-Test: {{{{id}}}}\n\n### users\n# @foreach user in {{{{$file users.json}}}}\nGET {base}/json\nX-Test: {{{{user.name}}}}\n"
    );
    let r = run_file("loop-file", &contents, &["-n", "ids", "-n", "users"]);

    assert_eq!(r.code, 0, "{}", r.stderr);
    let names = |slot: &Value| -> Vec<Value> {
        slot.as_array()
            .unwrap()
            .iter()
            .map(|it| echo_field(it, "x_test")[0].clone())
            .collect()
    };
    assert_eq!(names(&r.stdout[0]), [json!("7"), json!("8")]);
    assert_eq!(names(&r.stdout[1]), [json!("ann"), json!("bob")]);
    assert_eq!(r.stdout[1][1]["loop"], json!({"user": 1}));
}

#[test]
fn loop_file_errors_before_sending() {
    // A missing file is reported before "first" is sent (it would fail to
    // connect otherwise).
    let r = run_file(
        "loop-file-missing",
        "### first\nGET http://127.0.0.1:1/\n\n### each\n# @foreach id in {{$file nope.txt}}\nGET http://127.0.0.1:1/{{id}}\n",
        &["-n", "first", "-n", "each"],
    );
    assert_eq!(r.code, 1);
    let error = r.stderr["error"].as_str().unwrap();
    assert!(
        error.starts_with("request \"each\": @foreach {{$file nope.txt}}: failed to read "),
        "{error}"
    );
}

#[test]
fn looping_request_takes_one_json_slot() {
    let base = start_server();
    let r = run_file(
        "loop-slot",
        &loops(&base),
        &["-n", "list", "-n", "each", "-n", "none"],
    );

    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(r.stdout.as_array().unwrap().len(), 3);
    assert_eq!(r.stdout[0]["name"], "list");
    assert_eq!(r.stdout[1].as_array().unwrap().len(), 2);
    assert_eq!(r.stdout[1][0]["name"], "each");
    assert_eq!(r.stdout[2], json!([]));
}

#[test]
fn loop_default_and_body_output() {
    let base = start_server();
    let (code, stdout) = run_body("loop-default", &loops(&base), &["-n", "each"]);
    assert_eq!(code, 0);
    assert_eq!(response_count(&stdout), 3);

    // An empty array sends nothing; only the dependency is printed.
    let (code, stdout) = run_body("loop-empty", &loops(&base), &["-n", "none"]);
    assert_eq!(code, 0);
    assert_eq!(response_count(&stdout), 1);

    let (code, stdout) = run_body("loop-body", &loops(&base), &["-n", "each", "-b"]);
    assert_eq!(code, 0);
    let text = String::from_utf8(stdout).unwrap();
    assert_eq!(text.matches("\"x_test\"").count(), 2, "{text}");
    assert!(!text.contains("\"users\""), "{text}");
}

#[test]
fn loop_over_non_array_is_a_runtime_error() {
    let base = start_server();
    let r = run_file("loop-not-array", &loops(&base), &["-n", "not array"]);

    assert_error(&r, "@foreach: {{list.response.body.$.one}} is not an array");
}

#[test]
fn missing_element_value_names_the_element() {
    let base = start_server();
    let r = run_file("loop-missing", &loops(&base), &["-n", "missing"]);

    assert_error(
        &r,
        "request \"missing\" (element 0): current element has no value at {{user.nope}}",
    );
}

#[test]
fn referencing_a_looping_request_follows_it() {
    let base = start_server();
    let r = run_file("loop-reference", &loops(&base), &["-n", "uses each"]);

    assert_eq!(r.code, 0, "{}", r.stderr);
    let iterations = r.stdout.as_array().expect("a following -n prints an array");
    assert_eq!(iterations.len(), 2);
    assert_eq!(iterations[1]["loop"], json!({"user": 1}));
    assert_eq!(
        echo_field(&iterations[1], "x_test"),
        json!(["application/json"])
    );
}

#[test]
fn invalid_foreach_is_a_parse_error() {
    let r = run_request(
        "loop-invalid",
        "# @foreach user {{a.response.body.$}}\nGET http://x/",
    );

    assert_error(&r, "line 2: invalid @foreach");
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

// --- checks ----------------------------------------------------------------

/// Runs `-f <file with contents> args...` without adding `--json`, returning
/// the exit code, stdout and the stderr lines.
fn run_raw(test: &str, contents: &str, args: &[&str]) -> (i32, String, Vec<String>) {
    let file = http_file(test, contents);
    let output = Command::new(env!("CARGO_BIN_EXE_axon-cli"))
        .args(["-f", file.as_str()])
        .args(args)
        .env("HOME", temp_dir("empty-home"))
        .output()
        .unwrap();
    (
        output.status.code().expect("process exited normally"),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr)
            .lines()
            .map(str::to_string)
            .collect(),
    )
}

const CHECKS: &str = "\
### list
# @assert status == 200
# @assert {{response.body.$.users[1].name}} == \"bob\"
# @assert {{response.headers.Content-Type}} contains \"json\"
# @warn {{response.body.$.users}} !contains \"carl\"
# @warn {{response.body.$.one}} == \"y\"
# @notice {{response.cookies.theme}} != \"dark\"
GET BASE/list
";

#[test]
fn passing_and_reported_checks() {
    let base = start_server();
    let file = CHECKS.replace("BASE", &base);
    let (code, stdout, stderr) = run_raw("checks-levels", &file, &["-n", "list"]);

    assert_eq!(code, 0, "{stderr:?}");
    assert!(stdout.starts_with("< HTTP/1.1 200 OK\n"), "{stdout}");
    assert_eq!(
        stderr,
        [
            "WARN request \"list\": {{response.body.$.one}} == \"y\": got \"x\"",
            "NOTICE request \"list\": {{response.cookies.theme}} != \"dark\": got \"dark\"",
        ]
    );

    // --strict makes the warning fail the run, but not the notice.
    let (code, _, stderr) = run_raw("checks-strict", &file, &["-n", "list", "--strict"]);
    assert_eq!(code, 1);
    assert_eq!(stderr.len(), 3, "{stderr:?}");
    assert_eq!(stderr[2], r#"{"error":"1 check failed"}"#);
}

#[test]
fn checks_in_json_output() {
    let base = start_server();
    let file = CHECKS.replace("BASE", &base);
    let (code, stdout, _) = run_raw("checks-json", &file, &["-n", "list", "--json"]);

    assert_eq!(code, 0);
    let output: Value = serde_json::from_str(&stdout).unwrap();
    let checks = &output["checks"];
    assert_eq!(checks.as_array().unwrap().len(), 6);
    assert_eq!(
        checks[0],
        json!({"level": "assert", "check": "status == 200", "ok": true})
    );
    assert_eq!(
        checks[4],
        json!({"level": "warn", "check": "{{response.body.$.one}} == \"y\"", "ok": false, "error": "got \"x\""})
    );
    assert_eq!(checks[5]["level"], "notice");

    // A request without checks has no "checks" field.
    let (_, stdout, _) = run_raw(
        "checks-json-none",
        &format!("### t\nGET {base}/text\n"),
        &["-n", "t", "--json"],
    );
    let output: Value = serde_json::from_str(&stdout).unwrap();
    assert!(output.get("checks").is_none(), "{output}");
}

const FAILING: &str = "\
### missing
# @assert status == 200
# @assert {{response.body.$.id}} exists
GET BASE/missing

### uses missing
GET BASE/echo?type={{missing.response.headers.Content-Type}}

### uses uses missing
GET BASE/echo?u={{uses missing.response.body.$.method}}

### independent
GET BASE/text
";

#[test]
fn failed_assert_skips_dependents_only() {
    let base = start_server();
    let file = FAILING.replace("BASE", &base);
    let args = ["-n", "uses uses missing", "-n", "independent"];
    let (code, stdout, stderr) = run_raw("checks-skip", &file, &args);

    assert_eq!(code, 1);
    assert_eq!(
        stderr,
        [
            "FAIL request \"missing\": status == 200: got \"404\"",
            "FAIL request \"missing\": {{response.body.$.id}} exists: response body is not JSON: expected ident at line 1 column 2",
            "SKIP request \"uses missing\": \"missing\" failed its checks",
            "SKIP request \"uses uses missing\": \"uses missing\" failed its checks",
            r#"{"error":"2 checks failed, 2 requests skipped"}"#,
        ]
    );
    // The failing response and the independent one are still printed.
    assert!(stdout.starts_with("< HTTP/1.1 404 Not Found\n"), "{stdout}");
    assert!(stdout.ends_with("hello text"), "{stdout}");

    // With --json, a skipped -n takes its slot as null.
    let mut json_args = args.to_vec();
    json_args.push("--json");
    let (code, stdout, _) = run_raw("checks-skip-json", &file, &json_args);
    assert_eq!(code, 1);
    let output: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(output[0], Value::Null);
    assert_eq!(output[1]["body"], "hello text");
}

#[test]
fn every_loop_iteration_is_checked() {
    let base = start_server();
    let file = format!(
        "\
### list
GET {base}/list

### each
# @foreach user in {{{{list.response.body.$.users}}}}
# @assert {{{{response.body.$.method}}}} == \"GET\"
# @assert {{{{user.id}}}} == 2
# @notice {{{{user.name}}}} != \"bob\"
GET {base}/echo/{{{{user.id}}}}
"
    );
    let (code, stdout, stderr) = run_raw("checks-loop", &file, &["-n", "each", "-b"]);

    assert_eq!(code, 1);
    assert_eq!(
        stderr,
        [
            "FAIL request \"each\" (element 0): {{user.id}} == 2: got \"1\"",
            "NOTICE request \"each\" (element 1): {{user.name}} != \"bob\": got \"bob\"",
            r#"{"error":"1 check failed"}"#,
        ]
    );
    // Both iterations were sent.
    assert_eq!(stdout.matches("\"method\":\"GET\"").count(), 2, "{stdout}");
}

#[test]
fn invalid_checks_fail_before_sending() {
    let base = start_server();
    for (check, error) in [
        (
            "# @assert {{nope}} exists",
            "request \"r\": @assert {{nope}} exists: undefined variable: nope",
        ),
        ("# @warn status =~ 200", "failed to parse"),
    ] {
        let file = format!("### r\n{check}\nGET {base}/text\n");
        let (code, stdout, stderr) = run_raw("checks-invalid", &file, &["-n", "r"]);
        assert_eq!(code, 1);
        assert_eq!(stdout, "");
        assert_eq!(stderr.len(), 1, "{stderr:?}");
        let message: Value = serde_json::from_str(&stderr[0]).unwrap();
        let message = message["error"].as_str().unwrap();
        assert!(message.starts_with(error), "{message}");
    }
}

#[test]
fn if_checks_report_when_true() {
    let base = start_server();
    let file = format!(
        "\
### list
# @fail-if status >= 500
# @warn-if {{{{response.body.$.one}}}} == \"x\"
# @notice-if {{{{response.body.$.users[0].name}}}} == \"carl\"
GET {base}/list
"
    );
    let (code, _, stderr) = run_raw("checks-if", &file, &["-n", "list"]);
    assert_eq!(code, 0, "{stderr:?}");
    assert_eq!(
        stderr,
        ["WARN request \"list\": if {{response.body.$.one}} == \"x\": matched \"x\""]
    );

    // --strict counts a failed @warn-if like a failed @warn.
    let (code, _, stderr) = run_raw("checks-if-strict", &file, &["-n", "list", "--strict"]);
    assert_eq!(code, 1);
    assert_eq!(stderr[1], r#"{"error":"1 check failed"}"#);

    let (_, stdout, _) = run_raw("checks-if-json", &file, &["-n", "list", "--json"]);
    let output: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        output["checks"],
        json!([
            {"level": "assert", "check": "status >= 500", "ok": true, "if": true},
            {"level": "warn", "check": "{{response.body.$.one}} == \"x\"", "ok": false, "if": true, "error": "matched \"x\""},
            {"level": "notice", "check": "{{response.body.$.users[0].name}} == \"carl\"", "ok": true, "if": true},
        ])
    );
}

// --- nested loops ----------------------------------------------------------

const NESTED: &str = "\
### list
GET BASE/list

### pairs
# @foreach a in {{list.response.body.$.users}}
# @foreach b in {{list.response.body.$.users}}
GET BASE/json
X-Test: {{a.name}}-{{b.name}}

### each
# @foreach user in {{list.response.body.$.users}}
GET BASE/json
X-Test: {{user.name}}
X-Test: {{user.id}}

### per each
# @foreach t in {{each.response.body.$.x_test}}
GET BASE/json
X-Test: {{user.name}}:{{t}}

### after each
GET BASE/json
X-Test: {{each.response.body.$.x_test[0]}}-{{user.id}}

### checked each
# @foreach user in {{list.response.body.$.users}}
# @assert {{user.name}} == \"ann\"
GET BASE/json

### after checked
GET BASE/json
X-Test: {{checked each.response.body.$.method}}-{{user.name}}

### pairs missing
# @foreach a in {{list.response.body.$.users}}
# @foreach b in {{list.response.body.$.users}}
GET BASE/json
X-Test: {{b.nope}}

### two loops
GET BASE/json
X-Test: {{each.response.body.$.method}}{{pairs.response.body.$.method}}

### unknown source
# @foreach t in {{user.tags}}
GET BASE/json
";

fn nested(base: &str) -> String {
    NESTED.replace("BASE", base)
}

/// The `x_test` header values the server saw in each iteration.
fn x_tests(iterations: &Value) -> Vec<Value> {
    iterations
        .as_array()
        .expect("an array of iterations")
        .iter()
        .map(|iteration| echo_field(iteration, "x_test"))
        .collect()
}

#[test]
fn nested_loops_run_every_combination() {
    let base = start_server();
    let r = run_file("nested-pairs", &nested(&base), &["-n", "pairs"]);

    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(
        x_tests(&r.stdout),
        [
            json!(["ann-ann"]),
            json!(["ann-bob"]),
            json!(["bob-ann"]),
            json!(["bob-bob"])
        ]
    );
    assert_eq!(r.stdout[1]["loop"], json!({"a": 0, "b": 1}));
}

#[test]
fn following_a_loop_runs_per_iteration() {
    let base = start_server();
    let r = run_file(
        "nested-follow",
        &nested(&base),
        &["-n", "per each", "-n", "after each"],
    );

    assert_eq!(r.code, 0, "{}", r.stderr);
    // `each` runs once (as a dependency), then each follower per iteration.
    assert_eq!(
        x_tests(&r.stdout[0]),
        [
            json!(["ann:ann"]),
            json!(["ann:1"]),
            json!(["bob:bob"]),
            json!(["bob:2"])
        ]
    );
    assert_eq!(r.stdout[0][3]["loop"], json!({"user": 1, "t": 1}));
    assert_eq!(x_tests(&r.stdout[1]), [json!(["ann-1"]), json!(["bob-2"])]);

    // The default output prints the list, both `each` iterations, and the
    // four iterations that follow them.
    let (code, stdout) = run_body("nested-follow-raw", &nested(&base), &["-n", "per each"]);
    assert_eq!(code, 0);
    assert_eq!(response_count(&stdout), 7);
}

#[test]
fn failed_iterations_skip_only_what_follows_them() {
    let base = start_server();
    let (code, stdout, stderr) = run_raw(
        "nested-skip",
        &nested(&base),
        &["-n", "after checked", "--json"],
    );

    assert_eq!(code, 1);
    assert_eq!(
        stderr,
        [
            "FAIL request \"checked each\" (element 1): {{user.name}} == \"ann\": got \"bob\"",
            "SKIP request \"after checked\" (element 1): \"checked each\" (element 1) failed its checks",
            r#"{"error":"1 check failed, 1 request skipped"}"#,
        ]
    );
    let output: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(x_tests(&output), [json!(["GET-ann"])]);
}

#[test]
fn nested_loop_errors_name_every_variable() {
    let base = start_server();
    let r = run_file("nested-missing", &nested(&base), &["-n", "pairs missing"]);
    assert_error(
        &r,
        "request \"pairs missing\" (a 0, b 0): current element has no value at {{b.nope}}",
    );

    let r = run_file("nested-two", &nested(&refused_url()), &["-n", "two loops"]);
    assert_error(
        &r,
        "request \"two loops\": references more than one looping request: \"each\" and \"pairs\"",
    );

    let r = run_file(
        "nested-unknown",
        &nested(&refused_url()),
        &["-n", "unknown source"],
    );
    assert_error(
        &r,
        "request \"unknown source\": @foreach must loop over a {{NAME.response.body.$.path}} reference or an earlier loop variable: {{user.tags}}",
    );
}

#[test]
fn let_variables_per_iteration() {
    let base = start_server();
    let file = format!(
        "\
### list
GET {base}/list

### each
# @foreach user in {{{{list.response.body.$.users}}}}
# @let tag = user_{{{{user.id}}}}
# @let shout = {{{{tag}}}}-{{{{user.name}}}}
# @warn-if {{{{shout}}}} contains \"bob\"
GET {base}/json
X-Test: {{{{shout}}}}
"
    );
    let (code, stdout, stderr) = run_raw("let-loop", &file, &["-n", "each", "--json"]);

    assert_eq!(code, 0, "{stderr:?}");
    assert_eq!(
        stderr,
        [
            "WARN request \"each\" (element 1): if {{shout}} contains \"bob\": matched \"user_2-bob\""
        ]
    );
    let output: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        x_tests(&output),
        [json!(["user_1-ann"]), json!(["user_2-bob"])]
    );
}

#[test]
fn skip_if_leaves_out_iterations() {
    let base = start_server();
    let file = format!(
        "\
### list
GET {base}/list

### each
# @foreach user in {{{{list.response.body.$.users}}}}
# @let tag = user_{{{{user.name}}}}
# @skip-if {{{{tag}}}} contains \"ann\"
# @assert status == 200
GET {base}/json
X-Test: {{{{tag}}}}

### after each
GET {base}/json
X-Test: {{{{each.response.body.$.x_test[0]}}}}
"
    );
    let r = run_file("skip-if", &file, &["-n", "each", "-n", "after each"]);

    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(x_tests(&r.stdout[0]), [json!(["user_bob"])]);
    // The index of the iteration that ran is kept, showing the gap.
    assert_eq!(r.stdout[0][0]["loop"], json!({"user": 1}));
    // Followers run only for the iterations that were sent.
    assert_eq!(x_tests(&r.stdout[1]), [json!(["user_bob"])]);

    let (code, stdout) = run_body("skip-if-raw", &file, &["-n", "each"]);
    assert_eq!(code, 0);
    assert_eq!(response_count(&stdout), 2, "the list and one iteration");
}

#[test]
fn in_accepts_any_listed_status() {
    let base = start_server();
    let file = format!(
        "\
### missing
# @assert status in 200, 404
# @fail-if status !in 200, 201, 204
GET {base}/missing
"
    );
    let (code, _, stderr) = run_raw("in-status", &file, &["-n", "missing"]);

    assert_eq!(code, 1);
    assert_eq!(
        stderr,
        [
            "FAIL request \"missing\": if status !in 200, 201, 204: matched \"404\"",
            r#"{"error":"1 check failed"}"#,
        ]
    );
}

#[test]
fn imported_requests_run_and_list() {
    let base = start_server();
    let dir = temp_dir("imports");
    std::fs::create_dir_all(dir.join("shared")).unwrap();
    std::fs::write(
        dir.join("shared/common.http"),
        format!("@base = {base}\n\n### get list\n# @assert status == 200\nGET {{{{base}}}}/list\n"),
    )
    .unwrap();
    let main = dir.join("main.http");
    std::fs::write(
        &main,
        "import shared/common.http\n\n### first user\nGET {{base}}/json\nX-Test: {{get list.response.body.$.users[0].name}}\n",
    )
    .unwrap();
    let main = main.to_str().unwrap();

    let r = run_in(&temp_dir("empty-home"), &["-f", main, "-l"], &[]);
    assert_eq!(r.stdout, json!(["get list", "first user"]));

    let r = run_in(&temp_dir("empty-home"), &["-f", main], &[]);
    assert_eq!(
        r.stdout[0]["file"],
        dir.join("shared/common.http").to_str().unwrap()
    );
    assert_eq!(r.stdout[0]["start_line"], 3);

    // The imported request runs first, as a dependency.
    let r = run_in(
        &temp_dir("empty-home"),
        &["-f", main, "-n", "first user"],
        &[],
    );
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(r.echo("x_test"), json!(["ann"]));
}

#[test]
fn file_variables_are_relative_to_the_file_they_are_written_in() {
    let base = start_server();
    let dir = temp_dir("file-variables");
    std::fs::create_dir_all(dir.join("shared")).unwrap();
    std::fs::write(dir.join("shared/token.txt"), "abc123\n").unwrap();
    std::fs::write(dir.join("body.json"), "{\"k\": 1}\n").unwrap();
    std::fs::write(
        dir.join("shared/common.http"),
        "@token = {{$file token.txt}}\n",
    )
    .unwrap();
    let main = dir.join("main.http");
    std::fs::write(
        &main,
        format!(
            "import shared/common.http\n\n### r\nPOST {base}/json\nX-Test: {{{{token}}}}\n\n{{{{$file body.json}}}}\n"
        ),
    )
    .unwrap();

    let r = run_in(
        &temp_dir("empty-home"),
        &["-f", main.to_str().unwrap(), "-n", "r"],
        &[],
    );
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(r.echo("x_test"), json!(["abc123"]));
    assert_eq!(r.echo("body"), "{\"k\": 1}");
}

#[test]
fn missing_file_variable_is_reported_before_sending() {
    let r = run_request(
        "missing-file-var",
        "GET http://127.0.0.1:1/{{$file nope.txt}}",
    );
    assert_eq!(r.code, 1);
    let error = r.stderr["error"].as_str().unwrap();
    assert!(error.contains("nope.txt"), "{error}");
}

#[test]
fn file_variable_expands_tilde_to_home() {
    let base = start_server();
    let home = temp_dir("file-var-home");
    std::fs::create_dir_all(home.join(".ssh")).unwrap();
    std::fs::write(home.join(".ssh/id_test.pub"), "ssh-ed25519 AAAA test\n").unwrap();
    let file = http_file(
        "file-var-tilde",
        &format!("### r\nGET {base}/json\nX-Test: {{{{$file ~/.ssh/id_test.pub}}}}\n"),
    );

    let r = run_in(&home, &["-f", &file, "-n", "r"], &[]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(r.echo("x_test"), json!(["ssh-ed25519 AAAA test"]));
}
