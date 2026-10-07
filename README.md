# varyk-http

The official HTTP package for [Varyk](https://varyk.com), a language
for backend services that compiles to Rust: an HTTP server and an HTTP
client for Varyk services, on [axum](https://github.com/tokio-rs/axum),
[tower-http](https://github.com/tower-rs/tower-http), and
[reqwest](https://github.com/seanmonstar/reqwest).

varyk-http 0.2.0 is on [crates.io](https://crates.io/crates/varyk-http)
and works with varyk 0.8 (see [Versions](#versions)). Varyk is
experimental and pre-1.0: anything here may change.

The package covers what an ordinary production API needs and nothing
more: routes the compiler checks, hooks, safe defaults, errors that
never leak, WebSockets, server-sent events, uploads, a client, metrics,
and tests without a port. The design is in [`docs/specs/`](docs/specs/);
what the compiler does with routes is the HTTP section of Varyk's
[language reference](https://github.com/Varyk-Lang/varyk/blob/main/docs/language.md#http).

## Install

```sh
cargo install varyk --version '^0.8' --locked
varyk init users
cd users
varyk add http sql
```

`varyk add http sql` adds varyk-http as `http` and varyk-sql as `sql`,
so code writes `http::App` and `sql::connect`. varyk-sql's default
driver, SQLite, is compiled from C on the first build, which needs a C
compiler; its [README](https://github.com/Varyk-Lang/varyk-sql) has the
rest.

## A first service

A users API on a database, behind an API key, in one file.
`src/main.vr`:

```varyk
struct Config {
    database_url: string,
    api_key: string,
    #[default(3000)]
    port: u16,
    #[default("127.0.0.1")]
    address: string,
}

struct User {
    id: i64,
    name: string,
    created_at: Time,
}

struct NewUser {
    name: string,
}

struct State {
    db: sql::Pool,
    api_key: string,
}

async fn list_users(state: Shared<State>) -> Result<Vec<User>, Error> {
    state.db.all("select id, name, created_at from users order by id").await
}

async fn get_user(id: i64, state: Shared<State>) -> Result<Option<User>, Error> {
    state.db.first("select id, name, created_at from users where id = ?", id).await
}

async fn create_user(user: NewUser, state: Shared<State>) -> Result<http::Response, Error> {
    if user.name.is_empty() {
        return Err(http::bad_request("a user needs a name"));
    }
    let created_at = Time::now();
    let id: i64 = state.db.one("insert into users (name, created_at) values (?, ?) returning id", user.name, created_at).await?;
    let mut r = http::Response::json(User { id: id, name: user.name.clone(), created_at: created_at });
    r.set_status(201);
    r.set_header("location", format!("/users/{}", id));
    Ok(r)
}

async fn delete_user(id: i64, state: Shared<State>) -> Result<http::Response, Error> {
    let deleted = state.db.run("delete from users where id = ?", id).await?;
    if deleted == 0 {
        return Err(http::not_found("no such user"));
    }
    Ok(http::Response::empty())
}

async fn health(state: Shared<State>) -> Result<string, Error> {
    let _ok = state.db.run("select 1").await?;
    Ok("ok")
}

async fn check_key(req: http::Request, state: Shared<State>) -> Result<bool, Error> {
    match req.header("x-api-key") {
        Some(key) => Ok(key == state.api_key),
        None => Err(http::unauthorized("an x-api-key header is needed")),
    }
}

async fn open_db(url: string) -> Result<sql::Pool, Error> {
    let db = sql::connect(url).await?;
    db.migrate("migrations").await?;
    Ok(db)
}

fn build_app(state: Shared<State>) -> http::App {
    let mut app = http::App::new(state.clone());
    app.get("/health", health);
    app.get("/users", list_users);
    app.get("/users/{id}", get_user);
    app.post("/users", create_user);
    app.delete("/users/{id}", delete_user);
    app.before_on("/users", check_key);
    app
}

async fn start() -> Result<bool, Error> {
    let config: Config = env::parse()?;
    let db = open_db(config.database_url).await?;
    let mut app = build_app(Shared::new(State { db: db, api_key: config.api_key.clone() }));
    app.set_address(config.address);
    app.serve(config.port).await
}

async fn main() {
    if let Err(e) = start().await {
        log::error("{}", e);
    }
}
```

`migrations/0001_users.sql`:

```sql
create table users (
    id integer primary key,
    name text not null,
    created_at text not null
);
```

SQLite has no time type, so `created_at` is `text`, and varyk-sql
writes a `Time` into it as text of a fixed width and reads it back (its
[column types](https://github.com/Varyk-Lang/varyk-sql#column-types)).

`.env`:

```sh
DATABASE_URL=sqlite::memory:
API_KEY=dev-key
```

`varyk run` serves on `127.0.0.1:3000`:

```sh
curl -i -H 'x-api-key: dev-key' -d '{"name":"Ada"}' 127.0.0.1:3000/users
# 201, location: /users/1,
# {"id":1,"name":"Ada","created_at":"2026-10-07T12:00:00.123456Z"}
curl -H 'x-api-key: dev-key' 127.0.0.1:3000/users/1
# {"id":1,"name":"Ada","created_at":"2026-10-07T12:00:00.123456Z"}
curl -H 'x-api-key: dev-key' 127.0.0.1:3000/users/2   # 404 {"error":"not found"}
curl 127.0.0.1:3000/users                             # 401
curl 127.0.0.1:3000/health                            # "ok"
```

The same program, with its tests, is in [`demo/users`](demo/users).

A handler is an ordinary `async fn`. Its parameters are bound by name to
the path (`{id}`) and the query string, and by type to the JSON body,
the shared state, and the request. A path or query value is an integer,
`bool`, `string`, `Time`, or `Uuid`, and a query value may be an
`Option` of one, `None` when it is missing. A `Time` reads as
`Time::from_iso` reads it (`2026-10-07T12:00:00Z`, or with an offset,
taken to UTC), and a `Uuid` in its 36-character form. In a query string
a `+` reads as a space, as forms encode it, so an offset is sent as
`%2B`: `?since=2026-10-07T12:00:00%2B02:00`. The handler's return value is the
response: a value is JSON with 200, `None` is 404, nothing is 204, and
an `http::Response` is sent as built. `varyk check` checks every route
against its handler before anything builds. A path or query value that
does not parse as its type, a missing query value that is not an
`Option`, and a body that is not JSON of its type are each a 400 naming
the parameter, and the handler is not called. The language reference
has [the full rules](https://github.com/Varyk-Lang/varyk/blob/main/docs/language.md#routes-and-their-handlers).

## Settings

Called on the app before `serve`:

| Call | Default | Meaning |
|---|---|---|
| `app.set_address(addr)` | `"127.0.0.1"` | the IP address to listen on; `"0.0.0.0"` in a container |
| `app.set_body_limit(bytes)` | 2 MiB | a larger request body is a 413 |
| `app.set_timeout(ms)` | 30 000 | a request that takes longer is a 503 |
| `app.set_shutdown_grace(ms)` | 30 000 | on SIGTERM or ctrl-c, how long requests in flight have to finish |
| `app.set_idle_timeout(ms)` | 75 000 | how long a connection may wait for a request's headers, idle between kept-alive requests or still sending them, before it is closed (see [Production](#production)) |
| `app.set_max_in_flight(n)` | none | beyond `n` requests at once, a 503 `{"error":"server busy"}` |
| `app.allow_origin(origin)` | none | adds one CORS origin; `"*"` allows any |
| `app.compress()` | off | gzip for responses whose client accepts it |
| `app.metrics(path)` | off | Prometheus text at `path` (see [Metrics](#metrics)) |

`bytes`, `ms`, and `n` are `u64`. The address is an IP address, not a
host name: `"localhost"` does not work, `"127.0.0.1"` or `"::1"` does.
There is no limit on requests in flight unless `set_max_in_flight` sets
one, since memory is the platform's to manage. The body limit bounds
each request's body, and the timeout each request from its routing to
its response. A client that has not sent a request's headers within the
idle timeout, on a new connection or between the requests of a
kept-alive one, is disconnected. The in-flight limit counts a request until its
response starts, so open event streams, WebSockets, and file bodies
still being sent do not count.

CORS, once an origin is allowed, answers preflights for `GET`, `POST`,
`PUT`, `PATCH`, and `DELETE` with the headers `content-type` and
`authorization`, and never allows credentials. An origin is written as
a browser sends it, with no path: `https://app.example` or
`http://localhost:5173`.

A setting with a value that cannot work (a limit, timeout, or grace of
0, an address that is not an IP address, an origin that is not an
origin, `"*"` beside another origin, a metrics path that is not a route
path or is taken) makes `serve` an `Err` naming the setting, and every
`app.request` a 500. A setting called twice keeps the last value;
`allow_origin` adds.

`app.serve(port).await` logs `listening on http://127.0.0.1:3000
(set_address("0.0.0.0") to accept outside connections)`, the hint only
at `127.0.0.1`, and serves until SIGTERM or ctrl-c, then stops accepting, gives requests in
flight the shutdown grace, and gives `Ok(true)`. A setting that cannot
work, two routes the router cannot tell apart (`GET /users/{id}` and
`GET /users/{name}`), and a port that cannot be bound are each an
`Err`.

## Requests and responses

`http::Request`, as a handler or a hook takes it:

| Call | Gives |
|---|---|
| `req.method()`, `req.path()` | `string`; the path without its query string |
| `req.header(name)`, `req.cookie(name)` | `Option<string>`; a header name is matched without regard to case |
| `req.body()` | `string`, the body as text (empty when it is not UTF-8, and for a multipart form) |
| `req.body_bytes()` | `Bytes`, the body as it came, with no copy (empty for a multipart form) |

`http::Response`:

| Call | Gives |
|---|---|
| `http::Response::json(value)` | 200, `content-type: application/json` |
| `http::Response::text(s)` | 200, `content-type: text/plain; charset=utf-8` |
| `http::Response::bytes(b, content_type)` | 200, the body `b` as it is, with that `content-type` and `x-content-type-options: nosniff`, as a file has |
| `http::Response::empty()` | 204 |
| `http::Response::file(dir, name)` | the file `name` in the folder `dir`, with a content type from its extension |
| `r.set_status(code)`, `r.set_header(name, value)` | change it |
| `r.set_cookie(name, value, max_age)` | a cookie with `Path=/`, `HttpOnly`, `Secure`, `SameSite=Lax`, and `Max-Age` in seconds; `0` deletes it |
| `r.status()`, `r.header(name)`, `r.body()`, `r.read_json()` | read it, in a test or from the client; `r.body()` is empty when the body is not UTF-8 |
| `r.body_bytes()` | `Bytes`, the body `r.body()` reads, with no UTF-8 check: as it came, from `app.request` or the client; for a response a handler built, its text or bytes, a file's content read whole, and empty for an event stream |

A header that is not valid HTTP (the `content_type` of
`Response::bytes` among them), a status outside 100 to 599, and a
cookie name or value that could add attributes (a `;`, a comma, a
space, a quote) make the response a 500, with the reason logged.

A `Bytes` given to the JSON calls (`Response::json(b)`, the client's
`post(url, b)`) or returned by a handler is sent as a JSON string of
base64, as any value is. For a raw body, use `Response::bytes` and the
client's `post_bytes` and `put_bytes`.

`Response::file` serves a file from a folder, streamed as it is sent:

```varyk
async fn asset(name: string) -> http::Response {
    http::Response::file("public", name)
}
```

with `app.get("/assets/{name}", asset);`. A name that is absolute, has
an empty, `.`, or `..` part, has a part starting with `.`, or leads
outside the folder through a link is a 404 `{"error":"not found"}`,
as a missing file is, so a client cannot tell them apart. Every file
response carries `x-content-type-options: nosniff`, so a browser keeps
to the content type the extension gives (`html`, `css`, `js`, `txt`,
`json`, `svg`, `png`, `jpg`, `jpeg`, `gif`, `webp`, `ico`, `pdf`,
`wasm`; any other is `application/octet-stream`).

## Errors

| Call | Status |
|---|---|
| `http::bad_request(text)` | 400 |
| `http::unauthorized(text)` | 401 |
| `http::forbidden(text)` | 403 |
| `http::not_found(text)` | 404 |
| `http::conflict(text)` | 409 |
| `http::error(status, text)` | `status` |

An `Err` from a handler or a `before` hook with a status from 400 to
599 is sent with that status and `{"error":"<text>"}`: its text is
written for the client. Any other error, one passed on with `?` from a
database call, say, or one with a status outside 400 to 599, is a 500
with the fixed body `{"error":"internal error"}`, and its message is
logged with the method and path. A panic in a handler or a hook is the
same fixed 500. So a database message, a file path, or a secret inside
an error stays in the log.

## Hooks

```varyk
async fn check_key(req: http::Request, state: Shared<State>) -> Result<bool, Error> {
    match req.header("x-api-key") {
        Some(key) => Ok(key == state.api_key),
        None => Err(http::unauthorized("an x-api-key header is needed")),
    }
}

async fn stamp(req: http::Request, mut res: http::Response) {
    res.set_header("x-served-by", "users");
}
```

`app.before(f)` runs `f` before every route, `app.before_on("/admin",
f)` before the routes whose path, as the program wrote it, is `/admin`
or starts with `/admin/` (not `/administrators`), and `app.after(f)` on
every response the router makes. Each hook covers every route, wherever
its line is, and hooks run in the order they were added. A `before`
hook gives `Ok(true)` to let the request through, `Ok(false)` for a 403
`{"error":"forbidden"}`, or an `Err`, sent as above. Because
`before_on` tests the route the request matched, no spelling of a path
reaches an `/admin` route without its hook.

`before` hooks run after routing, so a request no route matches is a
404 without running any. A client can therefore tell a route that
exists (refused by its hook, 401 or 403) from one that does not (404).

## Live connections and uploads

A handler takes a WebSocket, an event stream, or a multipart form as a
parameter. A live handler returns nothing, or `Result<http::Response,
Error>` so that `?` works on its calls, as below; the response it
returns is ignored, since the connection was already answered, and an
`Err` is logged with the method and path at error level. The
exception is the package's own `Err` for a client that has gone, from a
`send` or a `close`, passed on unchanged as `?` does: it is logged at
debug level, since it is no fault of the service.
The connection closes when the handler returns.

### WebSockets

```varyk
async fn chat(ws: http::WebSocket) -> Result<http::Response, Error> {
    while let Some(text) = ws.recv().await? {
        ws.send(format!("you said: {}", text)).await?;
    }
    Ok(http::Response::empty())
}
```

with `app.get("/chat", chat);`. A request that is not a WebSocket
upgrade is a 400.

| Call | Gives |
|---|---|
| `ws.recv().await` | `Result<Option<string>, Error>`: the next text message, `None` once the client has closed or gone; a binary message closes the connection (take it with `recv_message`) |
| `ws.recv_message().await` | `Result<Option<http::live::Message>, Error>`: the next message, text or binary; `None` as `recv` gives it |
| `ws.send(text).await` | `Result<bool, Error>`; an `Err` once the connection is gone |
| `ws.send_bytes(b).await` | the same, for one binary message |
| `ws.close().await` | `Result<bool, Error>`: `true` closed, `false` when it was closed already |

A handler that takes binary messages reads them with `recv_message`,
whose `http::live::Message` is `Text(string)` or `Binary(Bytes)`:

```varyk
async fn echo(ws: http::WebSocket) -> Result<http::Response, Error> {
    while let Some(message) = ws.recv_message().await? {
        match message {
            http::live::Message::Text(text) => ws.send(text).await?,
            http::live::Message::Binary(b) => ws.send_bytes(b).await?,
        };
    }
    Ok(http::Response::empty())
}
```

`recv` takes text messages only, and the package answers pings. A
client's fault closes the connection, with `recv` or `recv_message`
giving `None` and the reason logged at debug level: a binary message
under `recv` with code 1003 (`recv_message` gives it instead), a
malformed message with 1002 (1007 for text that is not UTF-8), and a
message larger than the body limit with 1009. The handler's return
closes it normally (1000).

### Server-sent events

```varyk
async fn ticks(events: http::Sse) -> Result<http::Response, Error> {
    let mut i = 0;
    while i < 10 {
        events.send_event("tick", format!("{}", i)).await?;
        time::sleep(1000).await;
        i = i + 1;
    }
    Ok(http::Response::empty())
}
```

`events.send(text).await` sends one `data:` event, and
`events.send_event(name, text).await` one with an `event:` name; each
gives `Result<bool, Error>`, an `Err` once the client has gone. Text
with line breaks is sent as several `data:` lines of one event. The
package sends a comment every 15 seconds while the handler is quiet, so
proxies keep the connection open.

### Uploads

```varyk
async fn upload_avatar(id: i64, form: http::Multipart) -> Result<http::Response, Error> {
    while let Some(part) = form.next().await? {
        if part.name() == "avatar" {
            let _written = part.save_to("uploads", format!("{}.png", id)).await?;
        }
    }
    Ok(http::Response::empty())
}
```

with `app.post("/users/{id}/avatar", upload_avatar);`.

| Call | Gives |
|---|---|
| `form.next().await` | `Result<Option<http::live::Part>, Error>`: the next part, `None` after the last |
| `part.name()` | `string`, the form field's name |
| `part.file_name()` | `Option<string>`, the name the client gave a file |
| `part.content_type()` | `Option<string>`, the content type the client declared for the part, as it wrote it and not checked; `None` when it declared none, or one that is not visible ASCII |
| `part.text().await` | `Result<string, Error>`, the content as text |
| `part.bytes().await` | `Result<Bytes, Error>`, the content as it came |
| `part.save_to(dir, name).await` | `Result<u64, Error>`: writes the content to the file `name` in the folder `dir`, which must exist, and gives the bytes written |

Save an upload under a name and extension the program chooses, as
above, never under `part.file_name()`: it is whatever the client sent,
as `part.content_type()` is. `save_to` refuses a name by the rule of
`Response::file` and writes to a temporary file first, so a failed
upload leaves nothing behind. A part's content is read once, by `text`,
`bytes`, or `save_to`. What the handler reads counts against the body
limit: past it, the request is a 413 whatever the handler returns. What it leaves unread is never read. A request that is not `multipart/form-data` is a 400.

## The client

```varyk
struct Forecast {
    summary: string,
}

struct State {
    client: http::Client,
}

async fn forecast(state: Shared<State>) -> Result<Forecast, Error> {
    let r = state.client.get("https://weather.example/today").await?;
    if r.status() != 200 {
        return Err(http::error(502, "the weather service did not answer"));
    }
    r.read_json()
}

fn weather_client(key: string) -> http::Client {
    let mut client = http::Client::new();
    client.set_header("authorization", key);
    client
}
```

| Call | Gives |
|---|---|
| `http::Client::new()` | a client with no default headers |
| `client.set_header(name, value)` | a header sent with every request |
| `client.set_timeout(ms)` | default 30 000, for the whole exchange |
| `client.set_body_limit(bytes)` | default 10 MiB, the largest response body read |
| `client.get(url)`, `client.delete(url)` | `Result<http::Response, Error>` |
| `client.post(url, body)`, `client.put(url, body)`, `client.patch(url, body)` | the same, with `body` sent as JSON |
| `client.post_bytes(url, b, content_type)`, `client.put_bytes(url, b, content_type)` | the same, with `b` sent as it is, under that `content-type` |

A response of any status is `Ok`. An `Err` is a response that could
not be had: a URL that is not `http` or `https`, a content type that
is not valid HTTP, a connection or TLS failure, the timeout, or a body
over the limit. Its message names at most the scheme and host, never the path
or query, which may hold a token.
Redirects are followed, up to ten, only to the same scheme, host, and
port; any other is given back as its 3xx response. A setting that
cannot work (a header that is not valid HTTP, a timeout or limit of 0)
is the `Err` of every later call.

The client uses rustls with built-in root certificates, so it works in
a minimal container, and it ignores `HTTP_PROXY`, `HTTPS_PROXY`, and
`NO_PROXY`: it always connects directly. Make one client at startup and
keep it in the state; `client.clone()` shares its connections.

## Metrics

`app.metrics("/metrics")` serves Prometheus text at that path:
`http_requests_total` by `method`, `route` (the route's pattern,
`/users/{id}`, or `unmatched`), and `status`, and
`http_request_duration_seconds`, a histogram by `method` and `route`.
The metrics path answers only `GET` and `HEAD`, runs no hooks, and is
not counted.

The metrics path is open to anyone who can reach the service. A
service that listens beyond `127.0.0.1` keeps it from the public at its
proxy: the proxy does not route the path, or allows it only from the
scraper.

## Logging

The package logs through Varyk's `log`, which a program with an
`http::App` starts by itself: one line per request, after its response,
with the method, the path as received without its query string, the
status, and the time (`GET /users/1 200 3ms`); a 500's message at error
level; a live handler's `Err` at error level, except the package's
own `Err` for a client that has gone, which is at debug level; startup and shutdown at info level.
`LOG=debug` adds the reasons for refused file names and closed live
connections. A name or message a
client chose is written escaped, so it cannot start a log line of its
own.

## Testing

`app.request(req).await` runs one request through the same router,
settings, and hooks as `serve`, without a port, under `varyk test`:

```varyk
async fn test_app() -> Result<http::App, Error> {
    let db = sql::connect_with("sqlite::memory:", 1).await?;
    db.migrate("migrations").await?;
    Ok(build_app(Shared::new(State { db: db, api_key: "test-key" })))
}

#[test]
async fn creates_a_user() {
    let app = match test_app().await {
        Ok(app) => app,
        Err(e) => {
            assert_eq(e.message(), "no error");
            return;
        }
    };
    let mut req = http::Request::new("POST", "/users");
    req.set_header("x-api-key", "test-key");
    req.set_body("{\"name\":\"Ada\"}");
    let r = app.request(req).await;
    assert_eq(r.status(), 201);
    assert_eq(r.header("location"), Some("/users/1"));
}
```

`http::Request::new(method, path)` makes a request (the path may carry
a query string), and `req.set_header(name, value)`,
`req.set_body(text)`, and `req.set_body_bytes(b)`, for a body of bytes,
fill it in. An event stream's events are collected whole once its
handler returns, so `r.body()` gives them; a WebSocket route answers a
400, since no request `app.request` sends is an upgrade. Each `sqlite::memory:` pool is a database of its own, so every
test starts empty. Run `varyk test` from the package's folder, where
`migrations` and `.env` are.

## Production

**Configuration.** Read the port, the address, and secrets from the
environment with `env::parse`, as the first service does: in
development from `.env`, which `varyk init` keeps out of git, and in
production from the service's environment.

**Deploying.** In a container the service listens on `0.0.0.0`, so
connections from outside the container reach it: the first service
reads `ADDRESS` and calls `set_address("0.0.0.0")` with it. The runtime
image has Debian 12's C library, so the build stage is Debian 12 too:

```dockerfile
FROM rust:1-bookworm AS build
RUN cargo install varyk --version '^0.8' --locked
WORKDIR /src
COPY . .
RUN varyk build --release

FROM gcr.io/distroless/cc-debian12
WORKDIR /app
COPY --from=build /src/target/varyk/cache/release/users ./users
COPY migrations ./migrations
ENV ADDRESS=0.0.0.0
EXPOSE 3000
CMD ["./users"]
```

`COPY . .` sends the whole folder to the build, so a `.dockerignore`
beside the `Dockerfile` keeps the host's build output and secrets out
of it; `varyk init` writes this one since varyk 0.7.1:

```text
target
.env
```

Docker reuses the cached `cargo install` layer on a rebuild, so to pick
up a newer varyk, build once with `docker build --no-cache .`.

`DATABASE_URL` and `API_KEY` are set by the orchestrator. On SIGTERM
the service stops accepting and gives requests in flight the shutdown
grace, which a second signal ends at once; an open event stream holds
the shutdown until the grace ends, and a WebSocket ends with the
process.

**Idle connections.** The service closes a connection that has sent no
request for the idle timeout, 75 seconds unless `set_idle_timeout`
says otherwise, so a load balancer or proxy in front must close its
idle connections to the service sooner, or an occasional request meets
a closing connection and fails with a 502. AWS's Application Load
Balancer and nginx's upstream `keepalive_timeout` default to 60
seconds, which is sooner. Google Cloud's Application Load Balancers
keep idle backend connections for 600 seconds, which cannot be changed,
so behind one, set the idle timeout above that, as Google advises:
`app.set_idle_timeout(620000);`.

**TLS** is the proxy's or the load balancer's: the package serves plain
HTTP/1.1. So are security headers such as `strict-transport-security`
and `content-security-policy`; the package sets only
`x-content-type-options: nosniff`, on a file or bytes response.

**Health.** A `get("/health", health)` route whose handler runs
`db.run("select 1")`, as in the first service, outside any `before_on`
prefix, so the platform needs no key.

**Auth.** `before_on` with a hook that reads a header, as above. The
first service compares the key with `==`, which is not constant time;
a production check compares in constant time in a facade function of
the program (with `subtle::ConstantTimeEq`, say) or uses the bearer
tokens below. The package checks neither a body's `content-type` nor a
request's `Origin`: against a cross-site request, the defence is the
`SameSite=Lax` cookie `set_cookie` writes, which a browser does not
send on a cross-site `POST` or WebSocket; a service that takes cookies
some other way checks `Origin` in a `before` hook. A handler that needs
the user calls a function of the program, `let user =
current_user(req, state)?;`. Tokens and password hashes come from a
Rust facade of the program over
[jsonwebtoken](https://crates.io/crates/jsonwebtoken) and
[argon2](https://crates.io/crates/argon2), with `argon2 = "0.6"`,
`jsonwebtoken = { version = "11", features = ["rust_crypto"] }`, and
`serde = { version = "1", features = ["derive"] }` in the program's
`[dependencies]`. These crates need Rust 1.88 or later (jsonwebtoken
10 does not help: its `time` dependency needs 1.88 too).
`src/auth.rs`:

```rust
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use jsonwebtoken::{DecodingKey, EncodingKey, Header, Validation};

#[derive(serde::Serialize, serde::Deserialize)]
struct Claims {
    sub: String,
    exp: u64,
}

/// A hash of `password` to store; never store the password itself.
pub fn hash_password(password: &str) -> Result<String, varyk_std::Error> {
    match Argon2::default().hash_password(password.as_bytes()) {
        Ok(hash) => Ok(hash.to_string()),
        Err(e) => Err(varyk_std::Error::new(format!("cannot hash the password: {e}"))),
    }
}

/// Whether `password` is the one `hash` was made from.
pub fn password_matches(password: &str, hash: &str) -> bool {
    match PasswordHash::new(hash) {
        Ok(parsed) => Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

/// A token for `user`, signed with `secret`, valid for `seconds`.
pub fn sign(user: &str, secret: &str, seconds: u64) -> Result<String, varyk_std::Error> {
    let claims = Claims {
        sub: user.to_string(),
        exp: jsonwebtoken::get_current_timestamp() + seconds,
    };
    let key = EncodingKey::from_secret(secret.as_bytes());
    match jsonwebtoken::encode(&Header::default(), &claims, &key) {
        Ok(token) => Ok(token),
        Err(e) => Err(varyk_std::Error::new(format!("cannot sign the token: {e}"))),
    }
}

/// The user of an `authorization: Bearer <token>` header's value, or an
/// `Err` when the token is missing, expired, or not signed with `secret`.
pub fn user_of(header: &str, secret: &str) -> Result<String, varyk_std::Error> {
    let token = header.strip_prefix("Bearer ").unwrap_or_default();
    let key = DecodingKey::from_secret(secret.as_bytes());
    match jsonwebtoken::decode::<Claims>(token, &key, &Validation::default()) {
        Ok(data) => Ok(data.claims.sub),
        Err(e) => Err(varyk_std::Error::new(format!("the token is not valid: {e}"))),
    }
}
```

The program gives a refused token its 401 in Varyk, with `mod auth;`
and a `secret` in its state:

```varyk
fn current_user(req: http::Request, state: Shared<State>) -> Result<string, Error> {
    let header = match req.header("authorization") {
        Some(h) => h,
        None => "",
    };
    match auth::user_of(header, state.secret) {
        Ok(user) => Ok(user),
        Err(e) => {
            log::debug("{}", e);
            Err(http::unauthorized("a valid bearer token is needed"))
        }
    }
}
```

**Rate limiting** per client is the gateway's job. `set_max_in_flight`
is load shedding, a 503 beyond a number of requests at once, not a
limit per client; it does not count open event streams, WebSockets, or
file bodies still being sent.

**Metrics** stay behind the proxy (see [Metrics](#metrics)).

**Tests** run through `app.request` under `varyk test`, with no port
(see [Testing](#testing)).

## Versions

The compiler writes code that calls this package, and a program and
its packages must resolve to one `varyk-std`, so each minor varyk
release is followed by a varyk-http release.

| varyk-http | varyk |
|---|---|
| 0.1 | 0.7 |
| 0.2 | 0.8 |

## Security

Report a vulnerability as described in the
[security policy](https://github.com/Varyk-Lang/varyk-http/security/policy)
shared by every Varyk-Lang repository.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md); rules for AI coding agents are
in [AGENTS.md](AGENTS.md).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE)
or [MIT license](LICENSE-MIT) at your option. The name "Varyk" is a
trademark and is not covered by those licenses; see Varyk's
[TRADEMARKS.md](https://github.com/Varyk-Lang/varyk/blob/main/TRADEMARKS.md).
