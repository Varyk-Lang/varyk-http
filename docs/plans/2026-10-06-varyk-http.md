# varyk-http 0.1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build `varyk-http` 0.1: the HTTP server and client package of Varyk, a Varyk package (`src/lib.vr`) with a Rust facade over axum, tower-http, and reqwest, implementing the contract the compiler's milestone 5b4 adapters call, with WebSockets, server-sent events, multipart uploads, metrics, a demo, Varyk tests, and CI.

**Architecture:** `src/lib.vr` declares the surface and the error constructors in Varyk. `server.rs` holds `App`, its settings, the router build (positional paths, the conflict check), the one route wrapper (body read and limit, timeout, `before` hooks, the adapter task with its early-response channel, panics, `after` hooks), the fallbacks, the layers (request log, metrics, catch-panic, in-flight limit, CORS, compression), `serve`, and `request`. `message.rs` holds `Request`, `Response`, and the `respond_*` functions. `live.rs` holds `WebSocket`, `Sse`, `Multipart`, and `Part`. `client.rs` and `metrics.rs` hold the rest.

**Tech Stack:** Varyk (the compiler from `Varyk-Lang/varyk` branch `feat/milestone-5b4` until it merges, then `main`), Rust 1.85+, axum 0.8, tower 0.5, tower-http 0.6, tokio, reqwest 0.12 (rustls), multer 3, matchit 0.8.4, metrics 0.24 and metrics-exporter-prometheus 0.17, tokio-tungstenite 0.29.

**Spec:** `docs/specs/2026-10-06-varyk-http-design.md` ("spec 2.2" refers to its sections). The contract it implements is `Varyk-Lang/varyk`'s `docs/specs/2026-10-05-milestone-5b4-design.md` section 6 ("M5b4 §n"), as amended on 2026-10-06. Read both before any task.

## Global Constraints

- `AGENTS.md` of this repository applies in full: the contract items change only with a varyk release; a program never names axum, tower, or reqwest; a new handler parameter kind is added here; no crash on absence; no internal failure reaches a client; safe by default; getters and setters have different names; docs move with the code.
- The gate after every task, run from the repository root with the compiler built from the varyk checkout: `varyk check && varyk test && rustfmt --edition 2024 --check src/*.rs && (cd demo/users && varyk test)` (the demo part once task 7 creates it), then clippy in the assembled crate: `cd "$(varyk publish --assemble-only)" && cargo clippy --all-targets --config "patch.crates-io.varyk-std.path='$VARYK_STD_PATH'" -- -D warnings`.
- Local compiler: build it in `/Users/vlad/projects/varyk/varyk` (branch `feat/milestone-5b4`, which carries the 5b4 amendments) with `cargo build -p varyk`, use `/Users/vlad/projects/varyk/varyk/target/debug/varyk`, and set `VARYK_STD_PATH=/Users/vlad/projects/varyk/varyk/crates/varyk-std`.
- `varyk-std = "0.6"` (the version the compiler's branch carries) until the release change; no `serde`, `serde_json`, or `tracing` line: use `varyk_std::json`, `varyk_std::serde`, `varyk_std::tracing`.
- MSRV 1.85: no let-chains; `rust-version = "1.85"`.
- No `unwrap`, `expect`, indexing that can fail, or other crash-on-absence call in the facade's non-test code.
- Facade signatures that name a type of another `.rs` file write its full path (`crate::message::Request`).
- Every struct named at the root but `App`, `Response`, and `Client` is bound by type, so the root names exactly `App`, `Request`, `Response`, `Client`, `WebSocket`, `Sse`, `Multipart` (spec 2).
- Commits: conventional prefixes, no `<` or `>` in subjects, no attribution lines. Work on branch `feat/0.1`.

## Review Focus

1. A handler that returns `Err(e)` where `e` came from `?` on a database call: the client gets `{"error": "internal error"}` and the log line has the method, the percent-encoded path, and the message. Task 2.
2. A `before_on("/admin", ..)` hook and a request `GET /ADMIN/users` or `/admin/../admin/users`: the hook runs whenever the matched route's pattern is under `/admin`, and never runs for a route outside it. Task 3.
3. Two parts held at once in a multipart handler (`let a = form.next().await?; let b = form.next().await?;`), then `a.text()`: an `Err` (moved on), never a panic or a hang. Task 4.
4. A WebSocket handler with a path parameter that does not parse: a 400, never a 101 followed by a close. Task 6.
5. A client given a URL with a token in the query, failing to connect: the `Err` message names only the scheme and host. Task 5.

---

## Overview

Seven tasks. Task 1 scaffolds the package, the surface, the constructors, and CI. Task 2 builds the core: `Request`, `Response`, the `respond_*` functions, the router build, the route wrapper, the fallbacks, and `app.request`. Task 3 adds hooks, the layers, and the settings. Task 4 adds files and multipart. Task 5 adds `serve`, the client, server-sent events, and the in-flight limit's real-server test. Task 6 adds WebSockets and the Rust test. Task 7 adds the demo, the README, and the final repository docs.

## Context

- Spec sections: 2.1 (settings), 2.2 (binding and hooks), 2.3 (`Request`, `Response`), 2.4 (errors), 2.5 (live and uploads), 2.6 (client), 2.7 (metrics), 3 (safety, logging), 5 (layout, `Cargo.toml`), 6 (how it is built), 7 (tests, CI), 8 (release).
- The contract's Rust shapes, as the compiler calls them: `Varyk-Lang/varyk` `crates/varyk/src/backend/rust.rs` (`route_adapter` ~255, `hook_adapter` ~302, `bind` ~378, the registration ~1010-1045).
- The stub of the contract, the shape to match (its behaviour is a test double, not the real rules): `Varyk-Lang/varyk` `crates/varyk/tests/fixtures/packages/varyk-http/src/server.rs` and `src/lib.vr`.
- The sibling package for layout, CI, and release: `/Users/vlad/projects/varyk/varyk-sql` (`Cargo.toml`, `src/lib.vr`, `src/db.rs`, `src/tests.vr`, `demo/users`, `.github/workflows/ci.yml`, `release-please-config.json`).
- The language reference for what Varyk code and `.rs` signatures may say: `Varyk-Lang/varyk` `docs/language.md` ("Calling Rust", "HTTP", "Async functions and tasks", "Tests").
- This repository: `AGENTS.md`, `README.md`, `CONTRIBUTING.md`, `.github/workflows/release-please.yml`, `commit-messages.yml`.

## Development Approach

- TDD per task: Varyk `#[test]` functions in `src/tests.vr` first (through `app.request`, or a detached `serve` on a fixed loopback port per test where spec 7 says so), see them fail, implement, see them pass. Rust `#[cfg(test)]` tests only for the WebSocket conversation (task 6).
- Follow `varyk-sql` for the package shape, CI, and release; follow the spec's section 6 for every internal mechanism, and its section 2 for every visible behaviour.
- Each `.rs` file is one concern (spec 5); `server.rs` may split the router from the settings into a second private module if it grows past what one file holds clearly, reached through `src/lib.vr`'s `mod` lines.
- Not needed: HTTP/2, TLS in the server, OpenTelemetry, binary WebSocket messages, a request builder for the client (spec 9).
- Not in this plan: the release change of spec 7 (install varyk 0.7 from crates.io, drop the clippy patch, add `latest-varyk.yml`, `varyk-std` 0.7, the demo on `varyk-sql` 0.2); it is its own change once varyk 0.7 and `varyk-sql` 0.2 are on crates.io.

## Tasks

### Task 1: Scaffold, surface, constructors, CI

Spec 2 (the `lib.vr` listing), 2.4, 5, 7 (CI), 8.

**Files:**
- Create: `Cargo.toml`, `src/lib.vr`, `src/server.rs`, `src/message.rs`, `src/live.rs`, `src/client.rs`, `src/metrics.rs`, `src/tests.vr`, `testdata/hello.txt`, `.github/workflows/ci.yml`
- Modify: `.gitignore` (`target/`), `.release-please-manifest.json` only if needed

**Interfaces:**
- Produces: the module tree of spec 2 and 5 with the root names of the Global Constraints; the contract items as compiling shells with their final signatures (spec 6.1-6.3, M5b4 §6.1): `App::new<S: Send + Sync + 'static>(Arc<S>) -> App`, the route and hook methods `&mut self`, `async fn serve(&self, port: u16) -> Result<bool, varyk_std::Error>`, `async fn request(&self, req: crate::message::Request) -> crate::message::Response`, `route`/`before_hook`/`after_hook` taking async closures over `Request` (and `Response` for `after`), `Request`'s `param`, `query`, `json`, `state`, `bind`, and the five `respond_*` functions; the error constructors in `src/lib.vr` over `Error::with_status`.

- [ ] Write `Cargo.toml` per spec 5 (dependencies with default features off, `varyk-std = "0.6"`, `[lib] path = "src/lib.vr"`, the package metadata)
- [ ] Write `src/lib.vr` per spec 2 (`pub mod`, `mod metrics;`, `mod tests;`, the `pub use` lines, the six constructors)
- [ ] Write the facade files with the contract signatures and the smallest bodies that compile (a body may answer a fixed 500 until its task)
- [ ] Write the first `src/tests.vr` tests: each constructor gives its status and message (`e.status()`, `e.message()`)
- [ ] Write `.github/workflows/ci.yml` per spec 7: jobs `test` (stable) and `msrv` (1.85); install varyk from `Varyk-Lang/varyk`'s `main` with `cargo install --git` (as spec 7 and CONTRIBUTING.md say; this repository's pull request goes green once the compiler's PR #31 has merged); `VARYK_STD_PATH` from a checkout of the same `main`; `varyk check`, `varyk test`, rustfmt, clippy in the assembled crate with the `--config` patch, the demo's `varyk test` once it exists
- [ ] Run the gate

### Task 2: `Request`, `Response`, the router, and the route wrapper

Spec 2.1 (`request`), 2.2, 2.3 (all but `Response::file`, which is task 4), 2.4, 3, 6.1 (router build, positional paths, conflict check, fallbacks), 6.2, 6.3 (the wrapper: body read and limit, timeout, adapter task, early-response channel channel-first, join-error panics, abort guard, `respond_error`'s private message logged with method and path).

**Files:**
- Modify: `src/message.rs`, `src/server.rs`, `src/tests.vr`
- Test: `src/tests.vr`

**Interfaces:**
- Consumes: task 1's shells.
- Produces: a working `app.request` for routes without hooks or layers; `Response`'s full surface but `file`; cookies with their validation; the wrapper's extension points that tasks 3-6 use: the `before` hook list per route, the `after` hook call on the package `Response`, the early-response channel `bind` sends through, the route pattern stamped on the response for metrics; `set_body_limit` and `set_timeout` recorded and applied by the wrapper (their validation is task 3's).

- [ ] Write tests per spec 7's first group: each binding kind and return shape; a handler in a module under `src/tests/`; the 400s (path, query, missing query, bad body with the parser's message); 404 and 405 with `allow`; 413 for a declared length and for a body crossing a lowered limit; 503 for a handler slower than a lowered timeout; each constructor's status; no status as the fixed 500; a status outside 400 to 599 as the 500; a handler panic (an index past a `Vec`'s end) as the fixed 500; `GET /users/{id}` with `GET /users/{name}` as a 500 from `request`; an invalid header value and an invalid `Request::new` as a 500; cookies' attributes and `max_age` 0; a header name matched without case
- [ ] Implement `Request` (shared body buffer, `Clone + Send + Sync`), the contract methods, and `Response` (text body, `Send + Sync`), the `respond_*` functions, the cookie and header checks at send time
- [ ] Implement the router build in `request`: positional paths, the name maps, the conflict check, the 404 and 405 fallbacks
- [ ] Implement the route wrapper of spec 6.3: the body read and limit, the timeout, the adapter task, the early-response channel channel-first, the join-error panic, the abort guard, `respond_error`'s message logged with the method and the percent-encoded path (Review Focus 1)
- [ ] Run the gate

### Task 3: Hooks, layers, and settings

Spec 2.1 (settings table, validation), 2.2 (hooks, which responses they see), 2.7 (metrics), 3, 3.1, 6.1 (layer order, hooks placement, the request log middleware, the metrics recorder per app and its upkeep).

**Files:**
- Modify: `src/server.rs`, `src/metrics.rs`, `src/tests.vr`

**Interfaces:**
- Consumes: task 2's wrapper and router.
- Produces: `before`, `before_on`, `after` working; the settings `set_address`, `set_shutdown_grace`, `set_max_in_flight`, `allow_origin`, `compress`, `metrics` recorded, and every setting (task 2's `set_body_limit` and `set_timeout` included) validated (a bad value an `Err` from `serve`, a 500 from `request`).

- [ ] Write tests: `before`, `before_on`, `after` order; a hook registered after the route it covers; `before_on("/admin")` not matching `/administrators`, matched by pattern whatever the spelling (Review Focus 2); a request no route matches running no `before` hook; an `after` hook's header on a 404; a hook's `req.body()` equal to the handler's; CORS headers for an allowed origin and none for another, a preflight for `POST` with `content-type`; `"*"` with another origin as a bad setting; compression for a client that accepts gzip; `/metrics` text after two requests with the `method` label bounded and `route` the written pattern; each setting's bad value as a 500 from `request`
- [ ] Implement the hooks in the wrapper and the fallbacks, the request log middleware, the per-app metrics recorder and its labels, catch-panic with the fixed JSON body, the in-flight limit (global, load-shed, 503), CORS, compression, the settings and their validation
- [ ] Run the gate

### Task 4: Files and multipart

Spec 2.3 (`Response::file`, content types, streaming), 2.5 (`Multipart`, `Part`, `save_to`, limits, the 413), 3.1 (escaped names), 6.2 (the multipart stream kept for `bind`).

**Files:**
- Modify: `src/message.rs`, `src/live.rs`, `src/server.rs`, `src/tests.vr`
- Create: `testdata/` files and a committed symlink pointing out of it

**Interfaces:**
- Consumes: the wrapper's `bind` and body decision (task 2).
- Produces: `Response::file(dir, name)`; `Multipart` (`next`) and `http::live::Part` (`name`, `file_name`, `text`, `save_to`).

- [ ] Write tests: serving `testdata/hello.txt` with its content type; refusing `..`, an absolute name, a dot segment, and the committed link out of the folder, each a 404; multipart with two text parts and a file part saved under a chosen name in `target/`; a refused save name; a part read twice; two parts held at once, the first then read as an `Err` (Review Focus 3); a part over a lowered body limit as a 413; each multipart body's line breaks are built in Varyk from `json::parse` of a JSON string holding `\r\n` (Varyk has no `\r` escape and multer needs CRLF), with the boundary in a `content-type` header set by `req.set_header`
- [ ] Implement `Response::file` (streamed over `tokio::fs::File`, the content-type table, confinement with links followed) and the escaped name in its log line
- [ ] Implement `Multipart` and `Part` per spec 2.5 (multer, one live field behind the lock, `Part` as name, file name, index, and handle; the temporary file renamed into place; the over-limit flag the wrapper turns into a 413)
- [ ] Run the gate

### Task 5: `serve`, the client, server-sent events

Spec 2.1 (`serve`, the startup line, shutdown), 2.5 (`Sse`), 2.6 (client), 6.4, 7 (the real-server tests).

**Files:**
- Modify: `src/server.rs`, `src/client.rs`, `src/live.rs`, `src/tests.vr`

**Interfaces:**
- Consumes: tasks 2-4.
- Produces: `serve` (bind, the log line with the `set_address` hint, graceful shutdown raced against the grace, upkeep while metrics are on); `Client` (`new`, `set_header`, `set_timeout`, `set_body_limit`, `get`, `delete`, `post`, `put`, `patch`) deriving `Clone`; `Sse` (`send`, `send_event`, keep-alive), collected whole under `request`.

- [ ] Write tests with `app.serve(port).detach();` and `time::sleep(50).await` on a fixed loopback port per test: the client's `get`, `post` with a JSON body, a 404 as `Ok`, a timeout as `Err`, a body over the limit as `Err`, a redirect to the same origin followed and one to another origin returned, a connection failure whose message names only the scheme and host (Review Focus 5); three events read whole by `client.get`; with `set_max_in_flight(1)` and a slow handler, two started `client.clone().get(..)` calls giving one 200 and one 503; a bind failure on a port in use as an `Err`; and through `app.request`, an event stream's events collected in `r.body()`
- [ ] Implement `serve`, the client (rebuilt per setting, redirect policy, body limit, error messages), and `Sse` through `bind::<Sse>` and the early-response channel
- [ ] Run the gate

### Task 6: WebSockets

Spec 2.5 (`WebSocket`), 6.3 (the upgrade through the early-response channel), 7 (the Rust test).

**Files:**
- Modify: `src/live.rs`, `src/server.rs`, `src/tests.vr`

**Interfaces:**
- Consumes: the wrapper's channel and `bind` (task 2), `serve` (task 5).
- Produces: `WebSocket` (`recv`, `send`, `close`) through `bind::<WebSocket>`; a `#[cfg(test)]` module in `live.rs` using `tokio-tungstenite` under `varyk_std::run`.

- [ ] Write the Rust test (a server built through the package's own Rust API on a loopback port): text echoed; a binary message closing with 1003; a close; a WebSocket route with a path parameter that does not parse answering 400, not 101 (Review Focus 4); and the Varyk test: under `app.request` a WebSocket route answers 400
- [ ] Implement `WebSocket` (behind a lock, `&self` methods, ping/pong by the package, 1009 for an oversized message) and `bind::<WebSocket>` sending the 101 early
- [ ] Run the gate

### Task 7: The demo and the documents

Spec 4, 7 (the demo, README, CONTRIBUTING), 8.

**Files:**
- Create: `demo/users/Cargo.toml`, `demo/users/src/main.vr`, `demo/users/src/tests.vr` or a test module, `demo/users/migrations/` if used
- Modify: `README.md`, `CONTRIBUTING.md`, `AGENTS.md`, `.github/workflows/ci.yml` (the demo step); no `extra-files` entry for the demo (release-please would write `varyk-http`'s own version into its `varyk-std` line; the demo's line moves with the release change, as in `varyk-sql`)

**Interfaces:**
- Consumes: everything above.

- [ ] Write `demo/users`: the users API of spec 1 on `varyk-sql` 0.1 (list, get, create with 201 and `location`, delete), a `before_on` key check, `/health`, and its `#[test]`s through `app.request` on `sqlite::memory:`
- [ ] Write the README per spec 7: install, the fifteen-minute path, settings, errors, live connections and uploads (the `Result` form of a live handler), the client, metrics (open to anyone who can reach the service), testing, deploying (the `Dockerfile` with `set_address("0.0.0.0")`), the production notes of spec 4, that a route's existence can be told from a 404, and the version table (0.1 with varyk 0.7)
- [ ] Bring CONTRIBUTING.md and AGENTS.md in line with what was built (the gate, the install path, where things are)
- [ ] Run the gate, the demo included, then the 1.85 build of the assembled crate (`rustup run 1.85 cargo build` there with the patch)
