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
| `--strict` | Treat a failed `@warn` check like a failed `@assert` (see [Checks](#checks)) |

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

- A variable may use other variables, so `@base` can be built from `{{$env API_HOST}}` and used everywhere; switching environments only means changing the environment variable.
- A variable defined more than once takes its last definition everywhere, also inside other variables: with `@base = a`, `@api = {{base}}/v3`, `@base = b`, `{{api}}` is `b/v3`. A variable used in its own value takes the definition before it: `@path = {{path}}/more`.
- Keep secrets out of the `.http` file with `{{$env ...}}` (or use `.netrc`, see above).
- An undefined file variable or unset environment variable is an error (exit 1), reported before any request is sent, for the requests being run only.

### Imports

`import PATH` lines at the top of a file, before its first request, bring in another file's variables and requests (the JetBrains HTTP Client syntax), so shared values and requests can live in one place:

```http
# common.http
@sysapi = https://systemsapi.one.com/v3

### get cluster
# @assert status == 200
GET {{sysapi}}/find/clusters/by-name/{{cluster}}
```

```http
# webconfigs.http
import common.http

@cluster = cfaceface
@brand = {{get cluster.response.body.$.data[0].links.self | split("/", -3)}}

### webconfigs
# @foreach wc in {{webconfig.response.body.$.data}}
GET {{sysapi}}/brands/{{brand}}/clusters/{{cluster}}/webconfigs/{{wc.id}}
```

- PATH is relative to the importing file (or absolute). Imported files can import others; each file is included once, and an import cycle is an error.
- The imported variables and requests are inserted where the `import` line is. Imported requests can be run with `-n`, are listed by `-l` and `-f` (with a `"file"` field), and run as dependencies; their requests can use the importing file's variables (`{{cluster}}` above).
- A variable defined after the import replaces the imported one everywhere, including inside imported variables: if `common.http` has `@base = https://prod` and `@sysapi = {{base}}/v3`, a `@base = https://staging` after the import makes `{{sysapi}}` `https://staging/v3`.
- Errors name the file: `failed to parse common.http: line 12: ...`, `failed to read common.http (imported by webconfigs.http line 1): ...`. JetBrains' `run` lines are not supported; use `-n` and dependencies instead.

### Request-local variables

`# @let NAME = VALUE` before the request line defines a variable for that request only. It can use everything a request can (file variables, `$env`, responses, loop variables, filters and earlier `@let` lines) and is evaluated again for each loop iteration:

```http
### webconfigs
# @foreach wc in {{webconfig.response.body.$.data}}
# @let service = services_{{wc.id}}
# @let last = {{wc.links.self | split("/", -1)}}
# @warn-if {{last}} contains {{service}}
GET {{sysapi}}/brands/{{brand}}/clusters/{{cluster}}/webconfigs/{{wc.id}}
X-Service: {{service}}
```

- Visible in the request's URL, headers, body, checks and later `@let` lines; not in other requests (not even ones that follow it), file variables or `@foreach` sources.
- A plain `@name = value` line is a file variable, visible everywhere, wherever it is written; `@let` is a comment to other tools.
- Reusing a name, or the name of a file variable or loop variable, is an error, reported before any request is sent.

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
X-Csrf-Token: {{login.response.cookies.csrftoken}}
```

- `{{NAME.response.body.$.path}}` takes a value from the JSON body; `{{NAME.response.headers.Header-Name}}` takes a header (case-insensitive, first value if repeated); `{{NAME.response.cookies.cookie_name}}` takes the value of a cookie set by a `Set-Cookie` header (name case-sensitive, attributes and surrounding quotes removed, last value if set more than once). They work in the URL, headers and body, and inside file variables (`@token = {{login.response.body.$.token}}`).
- NAME is the referenced request's name, matched exactly as with `-n`; give a request a short name with `# @name`.
- The path supports `$`, `.key`, `['key']` and `[N]` (0-based), e.g. `$.items[0]['id']`. A string is inserted without quotes; numbers, booleans and `null` as written; objects and arrays as compact JSON.
- `axon-cli -f api.http -n me` runs `login` first: a referenced request that has not responded yet in this run is sent automatically, once, before the request that needs it (and its own dependencies before it). A request given with `-n` always runs, even if it already ran as a dependency.
- The default output prints every response sent, dependencies included; `-b` and `--json` print only the `-n` requests, so `-n me -b > me.json` and `-n me --json | jq .status.code` are unaffected by `login`. Add `--with-deps` to get the whole chain as JSON: `-n me --json --with-deps` prints `[{"name": "login", "dependency": true, ...}, {"name": "me", ...}]`.
- Unknown names, invalid references and cycles (`a` → `b` → `a`) are reported before any request is sent; a body that is not JSON, a missing path, header or cookie is an error when the request is about to be sent.

#### Cookies

Cookies set by one request are always sent back on later requests in the same run (when domain and path match), so a session cookie needs nothing extra. To use a cookie's value somewhere else, such as a CSRF header or the body, reference it:

```http
### login
POST {{base}}/login
Content-Type: application/json

{"user": "demo"}

### update profile
PUT {{base}}/profile
X-CSRF-Token: {{login.response.cookies.csrftoken}}
Content-Type: application/json

{"name": "Demo", "csrf": "{{login.response.cookies.csrftoken}}"}
```

If `login` responds with `Set-Cookie: csrftoken=x7k2p9; Path=/; SameSite=Strict`, the reference is `x7k2p9`. `axon-cli -f api.http -n "update profile"` runs `login` first, sends its cookies in the `Cookie` header, and puts the token in the header and the body. Only the final response's cookies can be referenced: a cookie set on a redirect is stored and sent back, but not captured unless that request runs with `--no-follow`.

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
- `{{user}}` is the current element, and `{{user.id}}`, `{{user['first name']}}`, `{{user.tags[0]}}` are values inside it, inserted like other references. The loop variable is letters, digits, `_` and `-`, visible in its own request and in requests that follow it (below), not in file variables, and must not have the same name as a file variable or another loop variable.
- An empty array sends nothing.
- The default output prints every iteration; `-b` prints each iteration's body, back to back. With `--json`, a looping `-n` takes one slot holding a flat array of its iterations, so `-n "list users" -n "user details"` prints `[{...}, [{...}, {...}]]` and a single looping `-n` prints just the array (`[]` when empty). Each iteration has `"loop": {"user": 2}` with the index of each loop variable.
- A `@foreach` must loop over a response body or a value inside an earlier loop variable, without filters, and a request can reference at most one looping request; these are reported before any request is sent. A value that is not an array, a missing `{{user...}}` value or a failed iteration stops the run with exit 1, naming the element: `request "user details" (element 2): ...`, or with several loop variables `request "domain details" (wc 2, domain 0): ...`.

#### Nested loops

Several `@foreach` lines nest, the first being the outermost, and the request runs for every combination. A later loop can read an earlier loop variable, or another response:

```http
### webconfig domains
# @foreach wc in {{webconfig.response.body.$.data}}
# @foreach domain in {{wc.attributes.domains}}
GET {{sysapi}}/webconfigs/{{wc.id}}/domains/{{domain.name}}

### webconfig x domain
# @foreach wc in {{webconfig.response.body.$.data}}
# @foreach domain in {{domains.response.body.$.data}}
GET {{sysapi}}/webconfigs/{{wc.id}}/domains/{{domain.attributes.name}}
```

The first runs once for each domain inside each webconfig; the second for every webconfig with every domain from the `domains` response.

#### Following a looping request

A request that references a looping request runs once for each of its iterations, after all of them, reading that iteration's response and keeping its loop variables:

```http
### domains per webconfig
# @foreach wc in {{webconfig.response.body.$.data}}
GET {{sysapi}}/webconfigs/{{wc.id}}/domains

### domain details
# @foreach domain in {{domains per webconfig.response.body.$.data}}
GET {{sysapi}}/webconfigs/{{wc.id}}/domains/{{domain.id}}
```

`-n "domain details"` fetches the domains of each webconfig, then the details of each of those domains, with `{{wc.id}}` the webconfig it came from. A request can follow a request that itself follows another. If an iteration fails an `@assert`, only the iterations that follow it are skipped: `SKIP request "domain details" (element 2): "domains per webconfig" (element 2) failed its checks`.

#### Skipping iterations

`# @skip-if CHECK` in a looping request skips the iterations where the check is true, like `continue`. It takes the same checks as [`@assert`](#checks), evaluated before sending, so it can use loop variables, `@let` variables and other responses, but not `{{response...}}`:

```http
# @foreach wc in {{webconfig.response.body.$.data}}
# @let service = services_{{wc.id}}
# @skip-if {{service}} contains "xyz"
# @skip-if {{wc.attributes.disabled}} == true
GET {{sysapi}}/brands/{{brand}}/clusters/{{cluster}}/webconfigs/{{wc.id}}
```

- One true `@skip-if` is enough. A skipped iteration is not sent, printed or checked, and requests that follow the loop do not run for it; with `--json` it is left out, and the `"loop"` indexes of the others show the gap.
- A value that cannot be read stops the run (exit 1), except with `exists` and `!exists`: `@skip-if {{wc.disabled}} exists`.
- Only looping requests (with `@foreach`, or following a loop) can use it; this, an invalid line or `{{response...}}` in it is reported before any request is sent.

### Filters

A `{{...}}` value can be passed through filters with `|`, applied left to right, to use part of a string:

```http
### create item
POST {{base}}/items
# responds with  Location: /api/v2/items/42?rev=7

### show item
GET {{base}}/items/{{create item.response.headers.Location | split("/", -1) | split("?", 0)}}
X-Revision: {{create item.response.headers.Location | split("rev=", 1)}}
Authorization: Bearer {{login.response.body.$.auth | trim | prefix("Bearer ")}}
```

| Filter | Result | `"/api/v2/items/42?rev=7"` becomes |
|---|---|---|
| `split("sep", N)` | Part N after splitting on `sep`; 0-based, negative counts from the end (`-1` is the last) | `split("/", 3)` → `items`, `split("/", -1)` → `42?rev=7` |
| `trim` | Leading and trailing whitespace removed | unchanged |
| `prefix("x")` | The text after `x`, which must be at the start | `prefix("/api/")` → `v2/items/42?rev=7` |
| `suffix("x")` | The text before `x`, which must be at the end | `suffix("?rev=7")` → `/api/v2/items/42` |

- Filters work on any value: file and environment variables, response references (body, header, cookie) and loop variables. In a file variable they apply when it is used: `@item_id = {{create item.response.headers.Location | split("/", -1)}}`.
- A JSON value that is not a string is filtered as its compact JSON text.
- Arguments are double-quoted strings, with `\"` and `\\` as the only escapes, and integers. A `|` inside quotes is not a filter separator, but `}}` cannot appear inside an argument.
- Unknown filters and wrong arguments are reported before any request is sent. A split index out of range or a missing prefix/suffix is an error when the request is about to be sent, naming the value: `{{... | split("/", 9)}}: split("/", 9): "/api/v2/items/42?rev=7" gives 5 parts`.
- `@foreach` cannot use filters, since it needs an array.

### Checks

`# @assert`, `# @warn` and `# @notice` lines before the request line check each response of that request:

```http
### get cluster
# @assert status == 200
# @assert {{response.body.$.data[0].links.self}} exists
# @warn {{response.headers.Content-Type}} contains "json"
# @notice {{response.body.$.data[0].deprecated}} !exists
GET {{sysapi}}/find/clusters/by-name/{{cluster}}
```

A check is `SUBJECT OPERATOR [EXPECTED]`:

- SUBJECT is `status` or one `{{...}}` value. Inside a check, `{{response.body.$.path}}`, `{{response.headers.Name}}`, `{{response.cookies.name}}` and `{{response.status}}` read the request's own response (for a looping request, the current iteration's). Variables, other requests' responses, loop variables and filters work as everywhere else.
- OPERATOR is `==`, `!=`, `in`, `!in`, `contains`, `!contains`, `<`, `<=`, `>`, `>=`, `exists` or `!exists`.
- `in` and `!in` take a comma-separated list, each value quoted or bare and possibly `{{...}}`: `status in 200, 404`, `{{response.body.$.state}} !in "deleted", "failed"`. Commas inside quotes or `{{...}}` do not split the list.
- EXPECTED is the rest of the line: a double-quoted string (`\"` and `\\` escapes) or bare text such as `200`, `true` or `{{wc.id}}`. `exists` and `!exists` take none.

Values are compared as text, as they would be inserted in a request (strings unquoted, other JSON values as compact JSON); `<`, `<=`, `>` and `>=` compare numbers. `exists` passes when the value can be read and its filters succeed, and a subject that cannot be read fails every operator except `!exists`.

| Check | Passes when |
|---|---|
| `# @assert status < 400` | the status code is below 400 |
| `# @assert status in 200, 404` | the status code is 200 or 404 |
| `# @assert {{response.body.$.error}} !exists` | the body has no `error` field |
| `# @assert {{response.body.$.data.id}} == "{{wc.id}}"` | the id matches the current loop element |
| `# @assert {{response.body.$.count}} >= 1` | `count` is a number of at least 1 |
| `# @warn {{response.headers.Location \| split("/", -1)}} == "42"` | the last part of `Location` is `42` |
| `# @notice {{response.cookies.session}} exists` | a `session` cookie is set |

Every check is evaluated, and each failure is printed on stderr after its response:

```
FAIL request "get cluster": status == 200: got "404"
SKIP request "webconfig": "get cluster" failed its checks
WARN request "webconfig": {{response.headers.Content-Type}} contains "json": got "text/plain"
NOTICE request "webconfigs" (element 2): {{response.body.$.data.deprecated}} !exists: got "true"
```

| Level | On failure |
|---|---|
| `@assert` | `FAIL`; exit code 1. Requests that use its response are not sent (`SKIP`), nor the requests that use theirs; other requests and loop iterations still run |
| `@warn` | `WARN`; exit code unchanged, unless `--strict` makes it act as `@assert` |
| `@notice` | `NOTICE`; never changes the exit code |

`@fail-if`, `@warn-if` and `@notice-if` take the same checks but report when the check is **true**, failing like `@assert`, `@warn` (including `--strict`) and `@notice`:

```http
# @fail-if status >= 500
# @warn-if {{wc.attributes.mail_send_username}} == null
# @warn-if {{wc.attributes.runtime.resource_limits.cpushares}} > 2
# @notice-if {{response.body.$.data.deprecated}} == true
```

```
WARN request "webconfigs" (element 3): if {{wc.attributes.mail_send_username}} == null: matched "null"
```

Only the comparison is inverted: a value that cannot be read, or that is not a number for `<`, `<=`, `>`, `>=`, is still reported, since the condition cannot be checked (`@warn-if {{x}} == null` also warns when `x` is missing). For `exists` and `!exists` a missing value simply does not exist, so `@fail-if {{response.body.$.error}} exists` passes when there is no `error`. With `--json`, their results have `"if": true`.

- When checks fail, the run ends with `{"error": "2 checks failed, 1 request skipped"}` on stderr and exit code 1. The responses are still printed, `--json` included.
- With `--json`, each response of a request with checks has `"checks": [{"level": "assert", "check": "status == 200", "ok": false, "error": "got \"404\""}, ...]`, passing checks included; a skipped `-n` request is `null`.
- Invalid check lines, filters, undefined variables and unknown requests in checks are reported before any request is sent. A request used in a check is a dependency like any other.

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

# Use a cookie from the login response; login runs first, and its cookies
# are also sent back automatically in the Cookie header
axon-cli -f api.http -n "update profile" -b | jq .

# Use the id from the Location header of "create item", which runs first
axon-cli -f api.http -n "show item" -b | jq .

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

`body` is the raw response body as a string, exactly as the server sent it (JSON bodies are not parsed or reformatted); a non-UTF-8 (binary) body is base64-encoded instead and marked with `"body_base64": true`, a field that is absent otherwise; several `-n` print a JSON array in the order given. With `--with-deps` the output is always an array of every response sent, dependencies included and marked `"dependency": true` (the field is absent otherwise), in the order sent. The exit code is `0` when every request sent received a response (including 4xx/5xx) and no check failed (see [Checks](#checks)), and `1` on failed checks or on errors (unknown or duplicate names, parse errors, undefined variables, invalid or cyclic response references, missing chained values, network errors), which are printed to stderr as `{"error": "..."}`. Names, variables and response references are checked before any request is sent; responses already printed stay printed when a later request fails (with `--json`, nothing is printed).

### Building

All dependencies are vendored in `vendor/` (wired up via `.cargo/config.toml`),
so builds run fully offline:

```sh
make release              # binary at target/release/axon-cli
make test
make vendor               # re-vendor after changing dependencies (needs network)
```
