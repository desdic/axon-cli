<p align="center">
  <img src="images/axon_cli.jpeg" alt="axoncli logo" width="600">
</p>

# axon-cli

A command-line HTTP client written in Rust that runs named requests from a `.http` file

---

## Usage

```
axon-cli -f <FILE>                        # list the requests in FILE
axon-cli -f <FILE> -l                     # list only the request names (#N for unnamed ones)
axon-cli -f <FILE> -n <NAME> [-n <NAME>]  # run the named requests, in the order given, printing headers and bodies
axon-cli -f <FILE> -n '#2'                # run the second request in the file (named or not)
axon-cli -f <FILE> -n <NAME> --json       # print the full responses as JSON
```

| Option | Description |
|---|---|
| `-f, --file FILE` | `.http` file to read requests from |
| `-n, --name NAME` | Request to run (repeatable): its name, or `#N` for the Nth request in the file (1-based, unnamed ones included); quote names with spaces: `-n "create item"` |
| `-l, --list` | Print only the request names, as a JSON array of strings; an unnamed request is listed as its `#N` |
| `--netrc-file PATH` | netrc file to read credentials from (default `~/.netrc`) |
| `--no-follow` | Do not follow redirects |
| `-j, --json` | Print each response as JSON (status, headers, cookies, body) instead of the headers and raw body |
| `-b, --body` | Print only the raw response body, without the status line and headers |

Credentials come from `.netrc` unless the request has an `Authorization` header or `user:pass@` in the URL. Cookies set by one request are sent on later requests in the same run.

### Examples

```sh
# List the request names
axon-cli -f api.http -l | jq -r '.[]'

# Run one request; prints the status line and headers, then the body as-is
axon-cli -f api.http -n "list items"

# Run the third request in the file, e.g. one without a name
axon-cli -f api.http -n '#3'

# Only the body, exactly as received (binary too)
axon-cli -f api.http -n "list items" -b | jq .items
axon-cli -f api.http -n "download" -b > file.bin

# Full response as JSON, extract the status code
axon-cli -f api.http -n "list items" --json | jq .status.code

# Create, then list; with --json several -n print an array in the order given
axon-cli -f api.http -n "create item" -n "list items" --json | jq -r '.[1].body'
```

### Output

Listing prints `[{"index", "name", "method", "url"}]` (URLs as written, before substitution; `name` is `null` for an unnamed request); `-l` prints `["name", ...]`, with `"#N"` in place of an unnamed request, so every entry can be passed to `-n` (listing is always JSON).

Running requests prints, for each response, the status line and headers prefixed with `< ` like `curl -v`, a `< ` line, then the body as raw bytes exactly as received (no trailing newline added):

```
< HTTP/1.1 200 OK
< content-type: application/json
< set-cookie: session=abc123; Path=/; HttpOnly; SameSite=Lax; Max-Age=3600
< 
{"items":[]}
```

Several `-n` print their responses in the order given; a newline is added between two responses when a body does not already end with one. With `-b`, only the bodies are printed, back to back with nothing added between them. With `--json`, running one request prints:

```json
{
  "index": 1,
  "name": "list items",
  "url": "https://api.example.com/items",
  "version": "HTTP/1.1",
  "status": { "code": 200, "reason": "OK" },
  "headers": { "content-type": ["application/json"] },
  "cookies": [
    { "name": "session", "value": "abc123", "domain": null, "path": "/",
      "expires": null, "max_age": 3600, "secure": false, "http_only": true, "same_site": "Lax" }
  ],
  "body": "{\"items\":[]}"
}
```

`body` is the raw response body as a string, exactly as the server sent it (JSON bodies are not parsed or reformatted); a non-UTF-8 (binary) body is base64-encoded instead and marked with `"body_base64": true`, a field that is absent otherwise; several `-n` print a JSON array in the order given. The exit code is `0` when every selected request received a response (including 4xx/5xx) and `1` on errors (unknown or duplicate names, parse errors, undefined variables, network errors), which are printed to stderr as `{"error": "..."}`. Names and variables are checked before any request is sent.

### Building

All dependencies are vendored in `vendor/` (wired up via `.cargo/config.toml`),
so builds run fully offline:

```sh
make release              # binary at target/release/axon-cli
make test
make vendor               # re-vendor after changing dependencies (needs network)
```
