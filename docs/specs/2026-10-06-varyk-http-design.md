# varyk-http: the HTTP package

Date: 2026-10-06.

**Status.** Implemented, not yet released. `varyk-http` is the official
Varyk package for HTTP: a server and a client, written in Varyk with a
facade over axum, tower-http, and reqwest. Its compiler side is Varyk
milestone 5b4 (`Varyk-Lang/varyk`,
`docs/specs/2026-10-05-milestone-5b4-design.md`, "M5b4 §n"), whose
section 6 is the contract this package implements: the items the
compiler's generated adapters call. It extends the earlier specs that
spec names (M5b3 for facades, M5b1 for async and `Shared`, M5a for
`Error`, JSON, and logging). Varyk is experimental and pre-1.0: anything
here may change.

## 1. Summary and scope

A program adds the package with `varyk add http sql` and writes a users
API in one file (M5b4 §1):

```varyk
struct User {
    id: i64,
    name: string,
}

struct State {
    db: sql::Pool,
}

async fn get_user(id: i64, state: Shared<State>) -> Result<Option<User>, Error> {
    state.db.first("select id, name from users where id = ?", id).await
}

async fn open_db() -> Result<sql::Pool, Error> {
    let db = sql::connect("sqlite://users.db?mode=rwc").await?;
    db.run("create table if not exists users (id integer primary key, name text not null)").await?;
    Ok(db)
}

async fn main() {
    let db = match open_db().await {
        Ok(db) => db,
        Err(e) => {
            log::error("cannot open the database: {}", e);
            return;
        }
    };
    let mut app = http::App::new(Shared::new(State { db: db }));
    app.get("/users/{id}", get_user);
    if let Err(e) = app.serve(3000).await {
        log::error("cannot serve: {}", e);
    }
}
```

The compiler checks the route against the handler and writes an adapter
that calls this package (M5b4 §2, §4). This spec is everything that runs:
the router and its settings, `Request` and `Response`, the error rules,
WebSockets, server-sent events, multipart uploads, the client, metrics,
the demo, the tests, and the release. The package aims at what an
ordinary production API needs and nothing more.

### 1.1 Decisions

- **axum and tower-http underneath, reqwest for the client** (M5b4 §1.1):
  top-tier performance, the tower-http middleware shelf, and the same
  tokio runtime `varyk-std` starts; axum's handler machinery never reaches
  a program, because the compiler writes every adapter.
- **Everything in 0.1.0.** The core server, the client, the demo,
  WebSockets, server-sent events, and multipart ship together; one
  `route` wrapper serves every kind of handler (section 6.3).
- **As much Varyk as the language allows.** What a program sees is
  declared in `src/lib.vr`; the error constructors are Varyk functions
  (section 2.4); the tests are Varyk (section 7). Rust is the facade:
  only what calls a generic crate API or the file system.
- **No JSON, serde, or tracing of its own.** The facade reads and writes
  JSON with `varyk_std::json`, bounds types with `varyk_std::serde`, and
  logs with `varyk_std::tracing`, the crates a program's own `json::` and
  `log::` calls use, so the two can never disagree on a version.
- **Defaults bound one request, not the service.** A body limit, a
  request timeout, and an idle timeout on every connection are on
  by default; there is no default limit on requests in
  flight, since memory is the platform's to manage, and
  `set_max_in_flight` is there for a service that wants to shed load.
- **Listening on `127.0.0.1` by default**, so a service run on a laptop
  is not open to its network; a container says `set_address("0.0.0.0")`.
- **Prometheus metrics through the `metrics` crates**, default features
  off.
- **Two compiler additions** (M5b4 amendments, made before this
  package): `Error::with_status(status, text)` as a Varyk call, and the
  compiler marking the `App` of the package being checked when that
  package is `varyk-http`, so the package's own tests can add routes.

## 2. Surface

Everything a program names is reached from the package's root. The
contract items (M5b4 §6.1) are in the same list; a program cannot call
the generic ones (V0108), and does not need to.

```varyk
// src/lib.vr
pub mod server;
pub mod message;
pub mod live;
pub mod client;
mod metrics;
mod tests;

pub use server::App;
pub use message::Request;
pub use message::Response;
pub use client::Client;
pub use live::WebSocket;
pub use live::Sse;
pub use live::Multipart;
pub use server::route;
pub use server::before_hook;
pub use server::after_hook;
pub use message::respond_empty;
pub use message::respond_json;
pub use message::respond_option;
pub use message::respond_error;
pub use message::respond_before;

pub fn bad_request(text: string) -> Error { Error::with_status(400, text.clone()) }
// unauthorized 401, forbidden 403, not_found 404, conflict 409 likewise
pub fn error(status: u16, text: string) -> Error { Error::with_status(status, text.clone()) }
```

Every struct named at the root but `App`, `Response`, and `Client` is
bound by type to a handler parameter (M5b4 §2.2 rule 3), so the root
names exactly `Request`, `WebSocket`, `Sse`, and `Multipart` besides
those three. Helper types (a multipart `Part`, the hook wrappers) stay in
their module (`http::live::Part`).

### 2.1 `App` and its settings

The intrinsics (`App::new`, the five route calls, `before`, `before_on`,
`after`) are the compiler's (M5b4 §2.1); `serve` and `request` are the
contract's (M5b4 §6.1). The settings are `mut self` methods, called
before `serve`:

| Call | Default | Meaning |
|---|---|---|
| `app.set_address(addr)` | `"127.0.0.1"` | the address to listen on; `"0.0.0.0"` in a container |
| `app.set_body_limit(bytes)` | 2 MiB | a larger request body is a 413 |
| `app.set_timeout(ms)` | 30 000 | a request that takes longer is a 503 |
| `app.set_shutdown_grace(ms)` | 30 000 | on SIGTERM or ctrl-c, how long requests in flight have to finish |
| `app.set_idle_timeout(ms)` | 75 000 | how long a connection may wait for a request's headers, idle between kept-alive requests or still sending them, before it is closed (section 6.4) |
| `app.set_max_in_flight(n)` | none | beyond `n` requests at once, a 503 `{"error": "server busy"}`; off unless set |
| `app.allow_origin(origin)` | none | adds one CORS origin; `"*"` allows any; see below |
| `app.compress()` | off | gzip for responses whose client accepts it |
| `app.metrics(path)` | off | Prometheus text at `path` (section 2.7) |

CORS, once an origin is allowed, answers preflights for the methods
`GET`, `POST`, `PUT`, `PATCH`, and `DELETE` and the headers
`content-type` and `authorization`, and never allows credentials; `"*"`
together with another origin is a setting that cannot work.

The in-flight limit counts a request until its response starts, so
open event streams, WebSockets, and file bodies still being sent do not
count.

`bytes`, `ms`, and `n` are `u64`; `addr`, `origin`, and `path` are
strings, read. A setting with a value that cannot work (a limit, a
timeout, or a grace of 0, an address that is not an IP address, an
origin that is not a URL origin as a browser sends it, `http` or
`https`, `://`, and a host in lower case with an optional port, a
metrics path that is not a route path without `{name}` parts or is
taken by a route) is
recorded and makes `serve` an `Err` naming the setting, and `request` a
500 with the reason logged, never a panic. A setting called twice keeps
the last value; `allow_origin` adds.

Always on, with no call: the request log (section 3.1), panic
containment (a panic in a handler or hook is a 500 with the panic
logged), and the body limit and the timeouts above, the idle timeout
among them: a client that has not sent a request's headers within it,
on a new connection or between the requests of a kept-alive one, is
disconnected (section 6.4).

`app.serve(port).await` binds `address:port`, logs `listening on
http://ADDRESS:PORT` at info level, with `(set_address("0.0.0.0") to
accept outside connections)` appended when the address is `127.0.0.1`,
and serves until SIGTERM or ctrl-c. The address and port are written as
a socket address wherever they are shown, here and in `serve`'s `Err`,
so an IPv6 address is bracketed (`[::1]:3000`). It then stops accepting, gives
requests in flight the shutdown grace (section 6.4), and gives
`Ok(true)`. A setting
that cannot work, a route conflict the router finds (M5b4 §6.1), and a
failure to bind (the port in use, a permission refused) are each an
`Err` whose message names the address and port and the reason; no other
failure ends `serve`.

`app.request(req).await` runs one request through the same router,
settings, hooks, and middleware as `serve`, without a port (M5b4 §2.5),
so a test sees exactly the response a client would. The body limit
applies to `req`'s body; the timeout applies; shutdown and the address do
not. There is no connection to upgrade under `request`, so a WebSocket
route answers the 400 of section 2.5; an event stream's events are
collected whole once its handler returns, so `r.body()` gives them.

### 2.2 Routes, hooks, and binding

As M5b4 §2.2 to §2.4 say. The package's part:

- A path parameter and a query parameter are parsed from their text by
  the type's own parsing (`i64`, `bool` as `true` or `false`, a string as
  it is after percent-decoding); one that does not parse, and a required
  query parameter that is missing, is a 400 whose body is `{"error":
  "<what>"}` naming the parameter and the problem.
- A body is read whole, up to the body limit, and parsed with
  `varyk_std::json::parse`; a body that is not JSON of the type is a 400
  whose message is the parser's (it names the line, column, and field),
  prefixed with the parameter's name.
- A path no route matches is a 404 `{"error": "not found"}`; a matched
  path with another method a 405 `{"error": "method not allowed"}` with
  an `allow` header.
- Every hook applies to every route of the app, whatever their order of
  registration; hooks run in registration order; `before_on(prefix, f)`
  runs for a request whose matched route's path pattern, as the program
  wrote it (`/admin/{id}`), starts with the prefix on whole segments, so
  no spelling of the request's path can skip it (M5b4 §6.1).
- `before` hooks run after routing, so a request no route matches (404,
  405) runs no `before` hook; a client can therefore tell a route that
  exists (refused by its hook) from one that does not (404), which the
  README says. A hook's `req.body()` is the request's body, read once and the
  same copy the handler gets; it is empty only for a multipart body
  (section 6.2) and for the 404 and 405
  fallbacks, where no body is read. Every `after` hook runs on every response
  the router makes: a handler's, a `before` hook's refusal, the
  package's 400, 404, and 405, and the early response of a live handler.
  A handler's panic, a body over the limit, and a request over the
  timeout are answered by the route wrapper (section 6.3), so their 500,
  413, and 503 pass through `after` hooks too. The responses of the
  layers around the router (503 for the in-flight limit, a CORS
  preflight, and the 500 of a panic in a hook) do not pass through
  `after` hooks.

### 2.3 `Request` and `Response`

`Request` (M5b4 §6.2):

| Call | Gives |
|---|---|
| `http::Request::new(method, path)` | a request for `app.request`; `path` may carry a query string |
| `req.method()`, `req.path()` | `string`; the path without its query string |
| `req.header(name)`, `req.cookie(name)` | `Option<string>`; a header name is matched without regard to case |
| `req.body()` | `string`, the body as text (empty when it is not UTF-8) |
| `req.set_header(name, value)`, `req.set_body(text)` | for `app.request` |

`Response`:

| Call | Gives |
|---|---|
| `http::Response::json(value)` | 200, `content-type: application/json` |
| `http::Response::text(s)` | 200, `content-type: text/plain; charset=utf-8` |
| `http::Response::empty()` | 204 |
| `http::Response::file(dir, name)` | the file `name` in the folder `dir`, 200 with a content type from the extension; see below |
| `r.set_status(code)`, `r.set_header(name, value)` | change it |
| `r.set_cookie(name, value, max_age)` | a `set-cookie` with `Path=/`, `HttpOnly`, `Secure`, `SameSite=Lax`, and `Max-Age` (seconds, an `i64`); `0` deletes the cookie |
| `r.status()`, `r.header(name)`, `r.body()`, `r.read_json()` | read it (a test's or a client's response); `read_json` takes its type from where the result goes and gives `Result<T, Error>` |

`Response::file` refuses a `name` that is absolute, has an empty, `.`,
or `..` segment, has a segment starting with `.`, or resolves (links
followed) outside `dir`; a refused name and a missing file are both a 404
`{"error": "not found"}`, the reason logged at debug level, so a client
cannot tell which. The file is read when the response is sent, streamed
in chunks over `tokio::fs::File` (`futures_util::stream::unfold`), not
held in memory. Its content type comes from a fixed table of common
extensions of `name`, in any case (`html`, `css`, `js`, `txt` as
`text/...; charset=utf-8`, `json`, `svg` as `image/svg+xml`, `png`,
`jpg`, `jpeg`, `gif`, `webp`, `ico` as `image/x-icon`, `pdf`, `wasm`),
with `application/octet-stream` for any other. The name is checked and
the file opened when `Response::file` is called, so the file sent is the
one checked. Every file response carries `x-content-type-options:
nosniff`, so a browser keeps to the content type the table gives. A read
error partway through a file ends the response, logged at error level.

A header name or value that is not valid HTTP, set with `set_header`,
makes the response a 500 at send time with the reason logged (M5b4
§6.2); `set_status` with a code outside 100 to 599, and `set_cookie`
with a name that is not a cookie token or a value with a character
outside cookie octets (a `;`, a comma, a space, a quote), likewise, so a
value read from a request cannot add attributes such as `Domain=`. A
method, path, or header given to `Request::new` or `req.set_header` that
is not valid HTTP is kept as given and makes `app.request` a 500 with
the reason logged, as a setting that cannot work does.

`Response` holds its body as text, as a file to open at send time, or as
an event stream; it is `Send` and `Sync`, since an adapter
lends it across an `.await` (M5b4 §6.1). `r.body()` gives the text
(for a client's response, empty when it is not UTF-8), the
file's content read whole for a file response (empty when it cannot be
read or is not UTF-8), and empty for an event stream a handler holds;
the response `app.request` gives holds a file's content and an event
stream's collected events as text, so a test reads them with
`r.body()`.

### 2.4 Errors with a status

The constructors are Varyk functions in `src/lib.vr` over the builtin
`Error::with_status(status, text)` (M5b4 amendment):

| Call | Status |
|---|---|
| `http::bad_request(text)` | 400 |
| `http::unauthorized(text)` | 401 |
| `http::forbidden(text)` | 403 |
| `http::not_found(text)` | 404 |
| `http::conflict(text)` | 409 |
| `http::error(status, text)` | `status` |

How an `Err(e)` from a handler or a `before` hook is sent (M5b4 §2.6):
with a status from 400 to 599, that status and `{"error": "<message>"}`;
with no status, or a status outside 400 to 599, a 500 with the fixed
body `{"error": "internal error"}`, and the message logged at error level
with the method and path. `Ok(false)` from a `before` hook is a 403
`{"error": "forbidden"}`.

### 2.5 WebSockets, server-sent events, uploads

Each is a struct the compiler binds by type (M5b4 §2.2 rule 3), so a
handler takes one as a parameter. Each is `Send` and `Sync` with `&self`
methods, its connection behind a lock, so a handler's parameter needs no
`mut`. A handler taking a `WebSocket` or an `Sse` returns nothing, or
`Result<http::Response, Error>` so that `?` works on its calls; the
response it returns is ignored (the early response was already sent),
and an `Err`, whatever its status, is logged with the method and path
at error level. The one exception is the package's own `Err` for a
client that has gone (from a `send`, from a `close`, or from a
WebSocket upgrade that never completed), passed on unchanged, as `?`
does: it is logged at debug level, since it is no fault of the
service. The route wrapper tells it apart by its message, which the
package records on the request when it gives it; a `None` from `recv`
records nothing. The connection is
open while the handler runs and closes when it returns. The README shows
the `Result` form. `bind::<Sse>` and `bind::<WebSocket>` answer early, so
a parameter that fails after them could no longer answer; the compiler
binds the package's types last, so a path, query, body, or state
parameter's 400 still comes first, and a handler should not take another
parameter that can fail after one of them.

**`ws: http::WebSocket`**, on a `get` route; a request that is not a
WebSocket upgrade is a 400.

| Call | Gives |
|---|---|
| `ws.recv().await` | `Result<Option<string>, Error>`: the next text message, `None` once the client has closed or gone away |
| `ws.send(text).await` | `Result<bool, Error>`: `true` sent; an `Err` when the connection is gone |
| `ws.close().await` | `Result<bool, Error>`: a normal close, `true`; `false` when it was closed already; an `Err` when the connection is gone |

Messages are text. A binary message from the client closes the
connection with code 1003 ("unsupported data") and makes `recv` give
`None`. Ping and pong are answered by the package: a ping, read by
`recv`, is answered on the next `recv` or `send`. A single message
larger than the body limit closes the connection with code 1009, and a
malformed message closes it with 1002, or 1007 for text that is not
UTF-8; each makes `recv` give `None` too, with the reason logged at
debug level only, since the fault is the client's. Any other failure to
read is an `Err`. When the handler returns, the
connection closes normally (1000). A closing connection reads on, on a
task of its own, for the client's close in reply, at most 5 seconds,
so the closing handshake completes without the handler waiting for it.

**`events: http::Sse`**, on a `get` route:

| Call | Gives |
|---|---|
| `events.send(text).await` | `Result<bool, Error>`: one `data:` event; an `Err` once the client has gone |
| `events.send_event(name, text).await` | the same, with an `event:` name |

A `text` with newlines is sent as several `data:` lines of one event, as
the format requires. An event name with a line break is an `Err`, with
nothing sent, since it would end the line it is written on. The
response is a 200 with `content-type: text/event-stream` and
`cache-control: no-cache`. The package sends a comment line every 15
seconds while the handler is quiet, so proxies keep the connection
open.

**`form: http::Multipart`**, on a `post`, `put`, or `patch` route; a
request that is not `multipart/form-data` is a 400 `{"error": "the body
is not a multipart/form-data form"}`, and one without a boundary a 400
too.

| Call | Gives |
|---|---|
| `form.next().await` | `Result<Option<http::live::Part>, Error>`: the next part, `None` after the last |
| `part.name()` | `string`, the form field's name |
| `part.file_name()` | `Option<string>`, the name the client gave a file |
| `part.text().await` | `Result<string, Error>`, the part's content as text; an `Err` when it is not UTF-8 |
| `part.save_to(dir, name).await` | `Result<u64, Error>`, writes the content to the file `name` in `dir` and gives the bytes written |

A part's content is read once, by `text` or `save_to`; reading it again,
or reading a part after `next` has moved on, is an `Err`. multer allows
one live field at a time, and a Varyk local lives to the end of its
block, so a `Part` holds only its name, its file name, its index, and a
handle to the `Multipart`'s shared state (an `Arc` of its lock):
`Multipart` keeps the one live field behind its lock, `next` drains and
drops it before asking for the next, and `text` and `save_to` read
through `Multipart`, an `Err` for a part whose index has moved on. Two
parts held at once are therefore fine. `save_to`
refuses a `name` by the rule of `Response::file` (an `Err`, nothing
written) and writes to a temporary file in `dir` first, renamed into
place when complete, so a failed upload leaves no partial file. The
request counts against the body limit as it is read: a request whose
declared length is over it is a 413 before the handler runs, and a part
whose content crosses it while being read is an `Err` to the handler
(what the handler leaves unread is never read, so it costs no memory and
is not counted); the stream marks the `Multipart`'s shared state as over the
limit, and when the handler returns, whatever it returns, the route
wrapper answers the 413 `{"error": "body too large"}` instead, with
nothing logged as an error, since the fault is the client's. Every
other `Err` of `next`, `text`, and `save_to` (a form that is not valid
multipart, a `text` that is not UTF-8, a part read twice or one the form
has moved past, a refused name, a failure to write) has no status, so a
handler passing it on with `?` gives the fixed 500 with the message
logged, and no text of multer's reaches the client.
`part.file_name()` is the client's word, never a
safe file name: the README says to choose the name to save under.

### 2.6 The client

| Call | Gives |
|---|---|
| `http::Client::new()` | a client with no default headers |
| `client.set_header(name, value)` | a header sent with every request (a token, an API key) |
| `client.set_timeout(ms)` | default 30 000, for the whole exchange |
| `client.set_body_limit(bytes)` | default 10 MiB, the largest response body read |
| `client.get(url)`, `client.delete(url)` | `Result<Response, Error>` |
| `client.post(url, body)`, `client.put(url, body)`, `client.patch(url, body)` | the same, `body` sent as JSON (the `&T: Serialize` shape, M5b4 §2.7) |

A response of any status is `Ok`: the caller reads `r.status()` and may
read the body. An `Err` (with no status) is a response that could not be
had: a URL that does not parse or is not `http` or `https`, a connection
or TLS failure, the timeout, or a body over the limit. Its message names
the scheme and host, never the path or query, which may carry a token.

Redirects are followed up to ten, and only to the same scheme, host, and
port (M5b4 §6.4); any other, and an eleventh, is given back as its 3xx
`Response`. A header name or value that is not valid HTTP, a timeout
of 0, and a body limit of 0 are settings that cannot work: the client
is not built, and every later call is an `Err` naming the setting (a
header by its name, never its value, which may be a secret).
Connections use rustls with the bundled web roots and HTTP/1.1, so the
client works in a minimal container. `Client` keeps its settings and
the reqwest client built from them: each setter rebuilds it and stores
the outcome, a `Result` of the reqwest client and `Error`, so a build
failure is the `Err` of every later call and no setter panics. `Client`
derives `Clone` (a bare
`#[derive(Clone)]`, the form the compiler reads, as `varyk-sql`'s `Pool`
does) around the built reqwest client, so `client.clone()` gives another
handle to the same connection pool, so a `Client` belongs in the
`Shared` state, made once at startup.

### 2.7 Metrics

`app.metrics("/metrics")` serves, at that path, Prometheus text with two
families:

- `http_requests_total`, a counter labelled `method` (`GET`, `HEAD`,
  `POST`, `PUT`, `PATCH`, `DELETE`, `OPTIONS`, or `other` for any other
  method a client sends), `route` (the route's path pattern,
  `/users/{id}`, or `unmatched` for a response no route made), and
  `status`;
- `http_request_duration_seconds`, a histogram with the same labels but
  `status`, with the buckets of the Prometheus clients' defaults (0.005,
  0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10 seconds).

Each `App` owns its own recorder, built with those buckets and written
to directly; the package never installs the `metrics` crate's global
recorder, which a process may install once, so two apps in one process
(tests among them) keep their counts apart. While metrics are on,
`serve` runs the recorder's upkeep every 5 seconds, which drains
histogram data so memory does not grow when nothing scrapes.

The metrics path itself is not counted, runs no hooks, and is answered
only on `GET` (and `HEAD`); another method is a 405 with `allow:
GET,HEAD`. It is open to anyone who can reach the service, said in
the README: a service that listens beyond `127.0.0.1` serves metrics on
an internal port or behind its proxy.

## 3. Safety

- **No internal message reaches a client.** An error without a status, a
  panic, a header that is not valid HTTP, a failure to read a file: a 500
  with a fixed body, the reason in the log (section 2.4). A database
  message passed up with `?` is never sent.
- **Input never reaches a log line or a metric label raw.** Request log
  lines and a 500's log line carry the path as received, still
  percent-encoded (so an encoded line break stays `%0A`), without its
  query string; metric labels carry the
  route pattern, never the path; client errors name only the scheme and
  host.
- **Files stay in their folder.** `Response::file` and `save_to` refuse
  names that leave it (section 2.3, 2.5).
- **Hooks cannot be skipped.** Every hook covers every route; `before_on`
  matches the path the router matched (section 2.2).
- **Safe defaults.** `127.0.0.1`, a body limit, a request timeout, and
  an idle timeout on every connection, so a client that sends its
  headers slowly or not at all cannot hold connections open, CORS off,
  compression off (compressing a secret beside attacker text leaks it),
  a CORS `*` that never allows credentials, cookies `HttpOnly`, `Secure`,
  `SameSite=Lax`, and the client's same-origin redirects. The package
  checks neither a body's `content-type` nor a request's `Origin`: the
  defence against a cross-site request is the `SameSite=Lax` cookie
  `set_cookie` writes, which a browser does not send on a cross-site
  `POST` or WebSocket, and a service that takes cookies some other way
  checks `Origin` in a `before` hook.
- **No panic, no `unwrap`.** Every failure in the facade is a
  `varyk_std::Error` or a fixed response; a panic in a program's handler
  is contained.

### 3.1 Logging

The package logs through `varyk_std::tracing`, which the program starts
(M5b4 §2.8 starts logging in any program with `App::new`). One line per
request at info level, after the response: method, path as received
(percent-encoded) without query,
status, and time in milliseconds (`GET /users/1 200 3ms`), written by
the package's own middleware (section 6.1). A 500's message at error
level with the method and path, written by the route wrapper, and a
panic in a hook at error level, written by the catch-panic layer; a
live handler's `Err` at error level, except the package's own `Err`
for a client that has gone, which is at debug level (section 2.5); a refused file name and a
closed live connection at debug level. Startup
and shutdown at info level. A name the client chose (a refused file
name, the name in `save_to`'s error message), a 500's message, and a
panic's message, which may hold either, are written escaped, as Rust's
`{:?}` writes a string, so a decoded line break cannot start a new log
line.

## 4. Production use

What a service does with the package, each covered by the README:

- **Configuration.** The port and address from the environment through
  `env::parse` into the program's config struct; `.env` for development.
- **Containers.** The `Dockerfile` of `varyk-sql`'s README, with
  `app.set_address("0.0.0.0")` and the port exposed; SIGTERM from the
  orchestrator gives requests in flight the shutdown grace.
- **TLS** is the proxy's or load balancer's; the package serves HTTP.
- **Health.** A `get("/health", health)` route whose handler runs
  `db.run("select 1")`; the README shows it.
- **Auth.** `before_on("/admin", check_key)` reading a header; a handler
  that needs the user calls a function of the program
  (`let user = auth(req, state)?;`, M5b4 §1.1). JWT and password hashing
  through a facade of the program over `jsonwebtoken` and `argon2`, shown
  in the README.
- **Rate limiting** per client is the gateway's; `set_max_in_flight` is
  load shedding, not rate limiting.
- **Metrics** on `/metrics`, behind the proxy (section 2.7).
- **Tests** through `app.request` in `varyk test`, no port (M5b4 §2.5).

## 5. Package layout

```text
varyk-http/
  Cargo.toml
  src/lib.vr          the surface: pub mod, pub use, and the error constructors
  src/server.rs       App, its settings, the router, serve, request, route, the hooks
  src/message.rs      Request, Response, cookies, files, the respond_ functions
  src/live.rs         WebSocket, Sse, Multipart, Part
  src/client.rs       Client
  src/metrics.rs      the metrics layer and the exporter
  src/tests.vr        the Varyk tests (section 7), with a module of its own under src/tests/ for the cross-module handler
  testdata/           files the tests serve: a text file, and a link pointing out of the folder
  demo/users/         the users API of section 1, on varyk-sql
  docs/specs/         this file
  README.md, AGENTS.md, CONTRIBUTING.md, LICENSE-MIT, LICENSE-APACHE
  .github/workflows/  ci.yml, release-please.yml, commit-messages.yml, and at the release latest-varyk.yml
  release-please-config.json, .release-please-manifest.json
```

No `tests/`, `examples/`, or `benches/` directory (V0401 for a Varyk
package). Each `.rs` file is one concern; `server.rs` may split the
router from the settings if it grows past what one file holds clearly.

`Cargo.toml`:

- `[package]` `name = "varyk-http"`, `edition = "2024"`, `rust-version =
  "1.85"`, dual license, `authors = ["Vlad Mickevic"]`, `description`,
  `repository`, `homepage = "https://varyk.com"`, `keywords`,
  `categories = ["web-programming::http-server",
  "web-programming::http-client"]`; `[lib] path = "src/lib.vr"`;
- `[dependencies]`, default features off wherever the crate has them:
  `varyk-std` with a minor-version requirement, `0.7`; `axum` 0.8.9
  with `tokio`, `http1`, and `ws`; `hyper` with `http1` and `server` and
  `hyper-util` with `tokio` and `service`, the versions axum brings, for
  `serve`'s connections (section 6.4); `tower-http`
  with `catch-panic`, `cors`,
  `compression-gzip`; `tokio` with the features the server, the route
  wrapper's adapter task, files, uploads, and shutdown need (`net`,
  `rt`, `signal`, `fs`, `io-util`, `sync`, `time`), `rt` named although
  `varyk-std` brings it, since the package spawns tasks of its own, and
  `io-util` for reading a file in chunks and writing an upload; `tower` with `limit`,
  `load-shed`, and `util` (a concurrency limit queues, so the 503 of
  `set_max_in_flight` needs load shedding in front of it); `futures-util`;
  `matchit`, the version axum uses; `multer`, so a `Part` can outlive the
  call that gave it, which axum's own multipart field cannot;
  `form_urlencoded`; `reqwest` 0.12 with `rustls-tls`; `metrics` and
  `metrics-exporter-prometheus` (no HTTP listener, no push gateway); and
  `tokio-tungstenite`, the version axum uses, with its `connect` and
  `handshake` features, under `[dependencies]` (the compiler refuses a
  `.rs` file's use of a dev-dependency, even in a `#[cfg(test)]`
  module), for the Rust WebSocket test (section 7), which runs under
  `varyk_std::run`, so no tokio `macros` feature is needed.

No `serde`, `serde_json`, or `tracing` line: those come through
`varyk-std` (section 1.1).

## 6. How it is built

### 6.1 The router

`App` holds the state (`Arc<dyn Any + Send + Sync>`), the routes and
hooks as registered, and the settings. Nothing is built until `serve` or
`request`. Then each route's path is given to axum with its
parameters named by position (`/users/{p0}/orders/{p1}`), and the
route's own names are kept to answer `param("id")`: axum merges the
methods of a path only when the path's text is identical, so `GET
/users/{id}` beside `DELETE /users/{user_id}`, which Varyk allows, would
be two insertions of one shape, and a panic. Before axum sees them, the routes are checked for conflicts: each
distinct positional path is inserted into `matchit` once, and a method
and positional path seen twice (`GET /users/{id}` and `GET
/users/{name}`, both `GET /users/{p0}`) is a conflict, as is a matchit
refusal; either is an `Err` from `serve` and a 500 from `request`, never
axum's panic. The tower layers are then stacked: the request log
outermost, then metrics, then catch-panic (so a panic in a hook is still
logged and counted; neither of the two package middlewares can panic),
then the in-flight limit when
set (tower's `GlobalConcurrencyLimitLayer`, one semaphore for the whole
app, behind load shedding and an error handler mapping its refusal to
503; the per-layer `ConcurrencyLimitLayer` would give each route its
own limit), and CORS and compression when asked. Catch-panic is built
with a handler that answers the fixed `{"error": "internal error"}` 500
and logs the panic, not tower-http's default text. The request log is a
few lines of the package's own middleware, not tower-http's trace layer
(whose response callback sees no request, and whose spans varyk-std's
log lines do not print): it reads the method and the path without its
query string, awaits the service, and logs one info event with them, the
status, and the time. `respond_error` logs nothing, since it has no
request: it keeps a 500's message in a private field of `Response`,
never sent, and the route wrapper logs it at error level with the method
and path when it turns the package's `Response` into axum's, also for an
adapter response it ignores after a live handler's early response. The
route wrapper also puts its route's pattern, as the program wrote it,
in the response for the metrics layer; a response the route wrapper did
not make (the 404 and 405 fallbacks', the in-flight limit's 503, a CORS
preflight's) is labelled `unmatched`. The timeout is the route
wrapper's too, not a layer's: it bounds the hooks, the body read, and
the adapter until a response, with tokio's timer, so a request over it
is a 503 `{"error": "request timed out"}` that keeps its route's label
and passes through `after` hooks; the `after` hooks run within what is
left of the deadline, in the route wrapper and the fallbacks alike, and
when they overrun it the 503 is sent without them. A live connection is
not timed once its early response is sent. `before`
hooks are given to each route's wrapper (section 6.3); `after` hooks run
on the package's own `Response`, before it becomes axum's, in the route
wrapper and in the two fallbacks (the 404, and the 405 set with axum's
method-not-allowed fallback), so a file or an event stream is never
read back to give a hook. A
route's pattern as the program wrote it labels its metrics and is what
`before_on` tests. `request` builds the router, the conflict check
included, on each call, so a route or setting added after a request is
in the next one.

A facade signature that names a type of another `.rs` file of the
package writes its full path (`crate::message::Request` in `server.rs`),
since the importer does not follow a `use` in a `.rs` file (M3 §4.4).

### 6.2 `Request` and the contract methods

`Request` is the axum request's parts with its body in a shared buffer,
so it is `Clone`, `Send`, and `Sync`, and `&self` methods can read the
body (M5b4 §6.1). The route wrapper cannot see which types a handler
binds, so it decides from the request: a `multipart/form-data` body is
kept as a stream for `bind::<Multipart>` to take, once, and `req.body()`
is empty for it; every other body is read whole, up to the body limit,
before the adapter runs. The body limit is the wrapper's, not a layer's:
a declared length over it, and a body that crosses it while it is read,
are each a 413 `{"error": "body too large"}`. A WebSocket upgrade is a `GET` with no body, so
reading it costs nothing; the request keeps the upgrade for
`bind::<WebSocket>` to take, once. The wrapper reads it with axum's
`WebSocketUpgrade` extractor, which takes hyper's upgrade from the
request, so a request without one, every request `app.request` sends
included, is not an upgrade, and `bind::<WebSocket>` answers it with
the 400. `param`,
`query`, `json`, `state`, and `bind` give `Result<value, Response>`, the
`Err` being the 400 or 500 of section 2.2; `state::<S>` downcasts the
app's state, a mismatch being a 500 logged as a compiler bug (it cannot
happen in a program `varyk check` accepted).

### 6.3 One wrapper for every handler

`route(adapter)` gives axum a handler. When the router is built, each
route's handler is given the `before` hooks that cover it, in
registration order. It builds the `Request` once (the body read, section
6.2), runs those hooks on it in its own future, stopping at the first
refusal, and only then makes a one-shot channel for an early response
and runs the adapter as a task.
A response sent through the channel always wins: the wrapper waits on
the channel and the task together, the channel first, and when the task
ends it takes a response waiting in the channel before the adapter's. `bind::<WebSocket>` sends the
101 upgrade response and waits for the upgrade; `bind::<Sse>` sends the
`text/event-stream` response whose body is fed by `events.send`;
`bind::<Multipart>` takes the body stream and sends nothing early. The
adapter's own response, for a live handler `respond_empty`, is then
ignored. A panic in the adapter task arrives as the task's join error:
the wrapper answers it with the fixed 500 and logs the panic's message,
and after an early response it only logs it. The task is aborted when
the client goes away before any response, so a dropped request cancels
its handler, as a dropped task does in Varyk (M5b1 §2.4); once an early
response is sent the abort is disarmed, since the live handler must run
on. At the timeout the task is aborted the same way, unless an early
response was sent, so a handler does not go on writing after its 503.

### 6.4 Shutdown

`serve` accepts connections itself and serves each, as a task of its
own, with hyper's HTTP/1 connection builder, with upgrades, a timer,
and the idle timeout as hyper's header-read timeout (which takes effect
only with a timer, and `axum::serve` sets none). The timeout runs
whenever a connection waits for a request's headers, so it closes a
connection whose client sends them slowly or not at all, and an idle
kept-alive one. Its default, 75 seconds, is nginx's own keep-alive
timeout, and longer than the 60 seconds an AWS Application Load
Balancer and nginx's upstream keep-alive hold an idle connection, so
such a proxy closes an idle connection before the service does and no
request meets a closing one. Google Cloud's Application Load Balancers
hold idle backend connections for 600 seconds, which cannot be changed,
so a service behind one sets the idle timeout above that
(`set_idle_timeout(620000)`, as Google advises); the README says so. A failure to accept that
is one connection's (aborted, reset, or refused) is logged at debug
level and skipped; any other is logged at error level and followed by
a pause of one second, raced against the signal, which `serve` polls
first, so it stops on a signal even while every accept fails. On
ctrl-c or, on Unix, SIGTERM, `serve` stops accepting and asks each connection to shut down gracefully: an idle one
closes at once, and one with a request in flight once it is answered. A
timer of the shutdown grace starts at the signal; when every connection
has ended, or the grace ends, or a second signal ends the grace at
once, `serve` gives `Ok(true)`, and whatever is
left ends when the process does, at once in a program whose `main`
returns after `serve`. An open event stream is a
response still being sent, so it holds the shutdown until the grace
ends; a WebSocket is not tracked once upgraded, and ends with the
process.

## 7. Testing and definition of done

- **Varyk tests in `src/tests.vr`**, run by `varyk test`, using
  `app.request` (no port) for:
  - each binding kind and return shape, a handler in another module of
    the package's tests, `before`, `before_on`, and `after`, a hook
    registered after the route it covers, `before_on` not matching
    `/administrators` for `/admin`, a request no route matches running
    no `before` hook, routes `GET /users/{id}` and `GET /users/{name}`
    giving a 500 from `request`, a WebSocket route answering 400 (no
    request `app.request` sends is an upgrade);
  - the 400s for a path parameter, a query parameter, a missing required
    query parameter, and a bad body; the 404 and 405 with `allow`; 413
    for a body over a lowered limit; 503 for a handler slower than a
    lowered timeout;
  - an error with each constructor's status, with no status (the fixed
    500), with a status outside 400 to 599; a panic in a handler;
  - cookies (`set_cookie` attributes, `max_age` 0), headers, an invalid
    header value as a 500; `Response::file` serving a file and refusing
    `..`, an absolute name, a dot segment, and a link out of the folder;
  - CORS headers for an allowed origin and none for another;
    compression for a client that accepts gzip; `/metrics` text after
    two requests; each setting's bad value as a 500 from `request`;
  - multipart: two text parts and a file part saved under a chosen name,
    a refused name, a part read twice, a part over a lowered body limit
    as a 413; `save_to` writes under the
    git-ignored `target/` (`varyk test` is run from the package's
    folder, as CONTRIBUTING.md and CI do),
    and `Response::file` serves from the committed `testdata/`;
  - server-sent events, the client, and `set_max_in_flight` against a
    real `serve` on a loopback port (a fixed port per test), started with
    `app.serve(port).detach();` (a detached task ends with the test) and
    given `time::sleep(50).await` to bind before the first call: three
    events read whole by `client.get`; with `set_max_in_flight(1)` and a
    slow handler, two started `client.clone().get(..)` calls, one a 200
    and the other a 503; the
    client's `get`, `post` with a JSON body, a 404 as `Ok`, a timeout as
    `Err`, a body over the limit as `Err`, a redirect to the same origin
    followed and one to another origin returned, an error message that
    names no path.
- **Rust tests** in the facade's `#[cfg(test)]` modules, which `varyk
  test` also runs, only for what a Varyk program cannot reach: the path
  as a log line writes it, in `src/message.rs`, an idle timeout of one
  second closing a connection that sends part of its headers and an
  idle kept-alive one, in `src/server.rs`, and a
  WebSocket
  conversation in `src/live.rs` (`tokio-tungstenite`
  as the client, a server built through the package's own Rust API on
  a fixed loopback port per test: text echoed and a ping answered, a
  binary message closing with 1003, text that is not UTF-8 closing
  with 1007, a message over the body limit closing with 1009, a close from either side and on the handler's
  return, and a route whose path parameter does not parse answering
  400, not 101).
- **`demo/users`** builds and its own `varyk test` passes on SQLite: list,
  get, create (201 with a `location` header), delete, a `before_on`
  key check, and a 404; its `main` serves.
- **CI**, in jobs named `test` (stable) and `msrv` (1.85), the names the
  branch ruleset requires: `varyk check`, `varyk test`, `rustfmt --check`
  on `src/*.rs`, `cargo clippy --all-targets -- -D warnings` in the crate `varyk publish
  --assemble-only` writes, and the demo's `varyk test`; `msrv` runs the
  check and the tests. Both install varyk 0.7 from crates.io, and the
  demo uses the published `varyk-sql` 0.2, the release for the same
  varyk. A weekly "Latest varyk" job, as `varyk-sql` has, tries the
  newest varyk on crates.io, so a red run says a release is due.
- No `unwrap`, `expect`, or other crash-on-absence call in the facade.
- CONTRIBUTING.md's "Where things are" says most tests send requests
  through `app.request` and the event-stream, client, and in-flight
  tests serve on a fixed loopback port.
- README: install, the fifteen-minute path, the settings table, errors,
  live connections and uploads, the client, metrics, testing, deploying
  (the `Dockerfile` with `set_address("0.0.0.0")`), production notes of
  section 4, and the version table.

Done when CI is green, the first release is on crates.io, and the
`varyk-http` items of M5b4's roadmap list are checked.

## 8. Repository and release

As `varyk-sql` (its spec §7): release-please with `release-type: rust`,
one package at the root, tag `v0.1.0`; the publish job runs `varyk
publish` with the `release` environment's token; no angle brackets in
commit subjects; the organisation's shared security policy. The first
release goes out after varyk 0.7 and `varyk-sql` 0.2 are on crates.io,
since the package needs the first and the demo the second.

## 9. Not in this version

HTTP/2 and TLS in the server (the proxy's); binary WebSocket messages and
bytes bodies (milestone 5c brings a bytes type); streaming request and
response bodies beyond `Response::file` and `save_to`; per-request
client headers and a request builder; client cookies and proxies; route
groups and a wrapping hook (M5b4 §11); OpenTelemetry; rate limiting per
client; serving a whole folder of static files; templates.

## 10. Open questions

- Should the client offer per-request headers through a request builder
  (M5b4 §11)?
- Should a folder of static files be served by one call
  (`app.files("/assets", "public")`)?
- Should metrics be exportable through OpenTelemetry beside Prometheus?
- When milestone 5c brings bytes, should WebSockets and bodies take them
  in the same calls (`send_bytes`, `Response::bytes`) or new ones?

## 11. Decisions

Taken with the user on 2026-10-06:

- One spec and one release, 0.1.0, for the server, the client, the demo,
  WebSockets, server-sent events, and multipart.
- Metrics through `metrics` and `metrics-exporter-prometheus`.
- As much Varyk as the language allows; JSON, serde, and tracing only
  through `varyk-std`; the error constructors in Varyk over a new
  `Error::with_status` builtin; the package's own `App` recognised by the
  compiler so its tests are Varyk.
- No default limit on requests in flight; the body limit and timeout stay
  on; the address stays `127.0.0.1` with a hint in the startup line.
