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
| `-j, --json` | Print each `-n` response as JSON (status, headers, cookies, body) instead of the headers and raw body |
| `--with-deps` | With `--json`, also print the responses of requests run only as dependencies (marked `"dependency": true`); the output is then always an array of every response, in the order sent. Requires `--json` |
| `-b, --body` | Print only the raw body of each `-n` response, without the status line and headers |

Credentials come from `.netrc` unless the request has an `Authorization` header or `user:pass@` in the URL. Cookies set by one request are sent on later requests in the same run.

### Variables

File variables (`@name = value`) and environment variables (`{{$env NAME}}`) can be used in the URL, headers and body:

```http
@base = https://{{$env API_HOST}}/v1
@token = {{$env API_TOKEN}}

### list items
GET {{base}}/items
Authorization: Bearer {{token}}
X-Request-User: {{$env USER}}

### create item
POST {{base}}/items
Content-Type: application/json

{"name": "widget", "owner": "{{$env USER}}"}
```

```sh
# Set the values for one run
API_HOST=api.example.com API_TOKEN=s3cret axon-cli -f api.http -n "list items"

# Or export them once for the shell session
export API_HOST=staging.example.com
export API_TOKEN="$(pass show api/staging-token)"
axon-cli -f api.http -n "create item" -b | jq .id
```

- A variable may use earlier variables, so `@base` can be built from `{{$env API_HOST}}` and used everywhere; switching environments only means changing the environment variable.
- Keep secrets out of the `.http` file with `{{$env ...}}` (or use `.netrc`, see above).
- An undefined file variable or unset environment variable is an error (exit 1), reported before any request is sent, for the requests being run only.

### Chaining requests

A request can use values from another request's response, with the VS Code REST Client syntax:

```http
### login
POST {{base}}/login
Content-Type: application/json

{"user": "demo"}

### me
GET {{base}}/me
Authorization: Bearer {{login.response.body.$.token}}
X-Session: {{login.response.headers.X-Session-Id}}
```

- `{{NAME.response.body.$.path}}` takes a value from the JSON body; `{{NAME.response.headers.Header-Name}}` takes a header (case-insensitive, first value if repeated). They work in the URL, headers and body, and inside file variables (`@token = {{login.response.body.$.token}}`).
- NAME is the referenced request's name, matched exactly as with `-n`; give a request a short name with `# @name`.
- The path supports `$`, `.key`, `['key']` and `[N]` (0-based), e.g. `$.items[0]['id']`. A string is inserted without quotes; numbers, booleans and `null` as written; objects and arrays as compact JSON.
- `axon-cli -f api.http -n me` runs `login` first: a referenced request that has not responded yet in this run is sent automatically, once, before the request that needs it (and its own dependencies before it). A request given with `-n` always runs, even if it already ran as a dependency.
- The default output prints every response sent, dependencies included; `-b` and `--json` print only the `-n` requests, so `-n me -b > me.json` and `-n me --json | jq .status.code` are unaffected by `login`. Add `--with-deps` to get the whole chain as JSON: `-n me --json --with-deps` prints `[{"name": "login", "dependency": true, ...}, {"name": "me", ...}]`.
- Unknown names, invalid references and cycles (`a` → `b` → `a`) are reported before any request is sent; a body that is not JSON, a missing path or a missing header is an error when the request is about to be sent.

### Loops

A request with `# @foreach VAR in {{NAME.response.body.$.path}}` runs once for each element of that JSON array:

```http
### list users
GET {{base}}/users

### user details
# @foreach user in {{list users.response.body.$.users}}
GET {{base}}/users/{{user.id}}
X-Name: {{user.name}}
```

- `axon-cli -f api.http -n "user details"` runs `list users` first (it is a dependency like any other reference), then `user details` once per user, in array order.
- `{{user}}` is the current element, and `{{user.id}}`, `{{user['first name']}}`, `{{user.tags[0]}}` are values inside it, inserted like other references. The loop variable is letters, digits, `_` and `-`, only visible in its own request (not in file variables), and must not have the same name as a file variable.
- An empty array sends nothing.
- The default output prints every iteration; `-b` prints each iteration's body, back to back. With `--json`, a looping `-n` takes one slot holding an array of its iterations, so `-n "list users" -n "user details"` prints `[{...}, [{...}, {...}]]` and a single looping `-n` prints just the array (`[]` when empty).
- A request can have one `@foreach`, it must loop over a response body, and other requests cannot reference a looping request; these are reported before any request is sent. A value that is not an array, a missing `{{user...}}` value or a failed iteration stops the run with exit 1, naming the element: `request "user details" (element 2): ...`.

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

# Run a request that uses the login token; login runs first, only "me" is printed
axon-cli -f api.http -n me -b | jq .

# Same, but include the login response too (marked "dependency": true)
axon-cli -f api.http -n me --json --with-deps | jq -r '.[] | "\(.name) \(.status.code)"'

# Fetch details for every user, collect the names
axon-cli -f api.http -n "user details" --json | jq -r '.[].body | fromjson | .name'

# Take the host and token from the environment for one run
API_HOST=staging.example.com API_TOKEN=s3cret axon-cli -f api.http -n "list items"
```

### Output

Listing prints `[{"index", "name", "method", "url", "start_line", "end_line"}]` (URLs as written, before substitution; `name` is `null` for an unnamed request; `start_line`..`end_line` are the 1-based lines of the request's block, from its `###` up to the next one); `-l` prints `["name", ...]`, with `"#N"` in place of an unnamed request, so every entry can be passed to `-n` (listing is always JSON).

Running requests prints, for each response (dependencies included), as it arrives, the status line and headers prefixed with `< ` like `curl -v`, a `< ` line, then the body as raw bytes exactly as received (no trailing newline added):

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

`body` is the raw response body as a string, exactly as the server sent it (JSON bodies are not parsed or reformatted); a non-UTF-8 (binary) body is base64-encoded instead and marked with `"body_base64": true`, a field that is absent otherwise; several `-n` print a JSON array in the order given. With `--with-deps` the output is always an array of every response sent, dependencies included and marked `"dependency": true` (the field is absent otherwise), in the order sent. The exit code is `0` when every request sent received a response (including 4xx/5xx) and `1` on errors (unknown or duplicate names, parse errors, undefined variables, invalid or cyclic response references, missing chained values, network errors), which are printed to stderr as `{"error": "..."}`. Names, variables and response references are checked before any request is sent; responses already printed stay printed when a later request fails (with `--json`, nothing is printed).

### Building

All dependencies are vendored in `vendor/` (wired up via `.cargo/config.toml`),
so builds run fully offline:

```sh
make release              # binary at target/release/axon-cli
make test
make vendor               # re-vendor after changing dependencies (needs network)
```
