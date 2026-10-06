# varyk-http

The official HTTP package for [Varyk](https://varyk.com), a language
for backend services that compiles to Rust: an HTTP server and an HTTP
client for Varyk services, on [axum](https://github.com/tokio-rs/axum),
[tower-http](https://github.com/tower-rs/tower-http), and
[reqwest](https://github.com/seanmonstar/reqwest).

varyk-http is not released yet. It is milestone 5b4 of Varyk's
[roadmap](https://github.com/Varyk-Lang/varyk/blob/main/docs/roadmap.md),
and its design is settled: the compiler side in Varyk's
[5b4 spec](https://github.com/Varyk-Lang/varyk/blob/main/docs/specs/2026-10-05-milestone-5b4-design.md),
the package side in `docs/specs/` here once written. Until the first
release lands, this repository holds the project's conventions and
nothing here builds. Varyk is experimental and pre-1.0: anything here
may change.

## What it will offer

A users API on a database is `varyk init`, `varyk add http sql`, one
file, and `varyk run` away:

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

- **A route table in one place.** `get`, `post`, `put`, `patch`, and
  `delete` with `{name}` segments. A handler is an ordinary async
  function: its parameters bind by name to the path and the query
  string, and by type to the JSON body, the shared state, and the
  request. `varyk check` checks every route against its handler before
  anything builds.
- **The return value is the response.** A value is JSON with 200,
  `None` is 404, nothing is 204, and `http::Response` sets a status,
  headers, and cookies when a route needs them.
- **Errors that carry a status.** `http::bad_request("...")` and the
  other constructors; an error without a status is a 500 whose message
  is logged and never sent.
- **Hooks for what every request needs.** `before` and `before_on` for
  auth and checks, `after` for response headers. Tracing, a body limit,
  a timeout, panic containment, an in-flight limit, and graceful
  shutdown are on by default; CORS, compression, metrics, and the
  listening address (`127.0.0.1` until set) by one call.
- **Tests without a port.** `app.request(..)` runs a request in process
  under `varyk test`.
- **A client in the same package.** `http::Client` with default headers
  and a timeout; `get`, `post`, `put`, `patch`, `delete`; a response for
  any status.
- **After the core**, as releases of this package with no compiler
  change: WebSockets, server-sent events, and multipart uploads.

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
