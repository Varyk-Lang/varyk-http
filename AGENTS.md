# Working on varyk-http

This file is for anyone, human or AI agent, making changes to this
repository. [CONTRIBUTING.md](CONTRIBUTING.md) has the details; this is
the short list of rules.

## What this is

varyk-http is the official HTTP package for Varyk: an HTTP server on
axum and tower-http, and an HTTP client on reqwest. It is a Varyk
package: `src/lib.vr` is everything a program sees, the `.rs` files
beside it are the facade over those crates and the only Rust,
`src/tests.vr` holds the tests (the Rust tests, for what a Varyk
program cannot reach, are in the facade's `#[cfg(test)]` modules: a
WebSocket conversation in `src/live.rs`, the logged path in
`src/message.rs`, and the idle timeout in `src/server.rs`, which
`varyk test` also runs), and `demo/users` is the users API of
the fifteen-minute path. The design is in two places: the compiler side,
how a Varyk function becomes a route handler and what the compiler
checks and generates for it, is milestone 5b4 of Varyk,
[`docs/specs/2026-10-05-milestone-5b4-design.md`](https://github.com/Varyk-Lang/varyk/blob/main/docs/specs/2026-10-05-milestone-5b4-design.md)
in the compiler repository; the package side, the server and client
surface, settings and defaults, live connections and uploads, is this
repository's `docs/specs/`. Read both before changing what the package
offers.

## The gate

Run the checks CONTRIBUTING.md lists ("Build and test") before every
pull request: `varyk check`, `varyk test`, the demo's `varyk test` in
`demo/users`, rustfmt on the `.rs` files, and clippy on the crate
`varyk publish --assemble-only` writes. Until varyk 0.7 is on
crates.io, the compiler comes from varyk's `main` with
`VARYK_STD_PATH` set to the same checkout's `crates/varyk-std`, and
clippy is given that path as a `--config` patch. CI's `test` and
`msrv` jobs are required checks: keep those job names, and keep the
code building on Rust 1.85 (no let-chains).

## Rules

- **The compiler writes against a contract.** Section 6 of the 5b4 spec
  names the items the generated Rust of every program calls: `App` and
  its methods, `Request` with `param`, `query`, `json`, `state`, and
  `bind`, `Response`, `route`, `before_hook`, `after_hook`, and the
  `respond_*` functions, with the receivers and the `Send`, `Sync`, and
  `Clone` bounds that section fixes. Their
  names and signatures change only together with a varyk release, as a
  breaking change, with the README's version table updated. Anything
  else is this package's to change.
- **A program never names axum, tower, or reqwest.** The generated Rust
  of a program reaches this package through its own key only. What an
  adapter needs is defined or re-exported here; a Varyk program using
  this package writes no Rust.
- **A new kind of handler parameter is added here, not in the compiler.**
  Every struct named at the package's root other than `App`,
  `Response`, and `Client` is bound by type, so `bind` must accept it;
  helper types (`Part`, the hook wrappers) stay in their module. How
  `bind` bounds its types is this package's business. `WebSocket`,
  `Sse`, and `Multipart` are bound that way.
- **No crash on absence.** The facade never panics: no `unwrap`,
  `expect`, or indexing that can fail. Every failure is a
  `varyk_std::Error`, and a panic that escapes a handler is caught and
  answered as a 500.
- **No internal failure reaches a client.** An error without a status is
  a 500: its message is logged with the method and path and not sent,
  and the body is fixed. An error with a status from 400 to 599 sends its message; any other
  status is a 500 too. A
  response never carries a secret, a backtrace, a file path, or the text
  of a dependency's error, except the JSON parser's message about a body
  the client sent, in a 400, which describes only the client's own
  input.
- **Safe by default.** The package faces the network. Request tracing,
  a body limit, a request timeout, panic containment, and graceful
  shutdown are on without being asked; CORS, compression, and a limit on
  requests in flight are off until asked; the server listens on `127.0.0.1`
  until the program sets an address; hooks cover every route whatever
  the order they were added in; the client follows a redirect only to
  the same scheme, host, and port; a cookie is always HttpOnly, Secure,
  and SameSite=Lax. Where the easy way and the safe way differ, the safe
  way is the default and the other is asked for by name.
- **Getters and setters have different names** (`status()` and
  `set_status(code)`): an imported Rust struct cannot have two methods
  of one name, and the surface stays as plain as Go's.
- **Commits.** Conventional prefixes (`feat:`, `fix:`, `docs:`, ...),
  no `<` or `>` in subjects or `BREAKING CHANGE:` footers, and no
  attribution lines (no `Co-Authored-By`, no "Generated with").
- **The demo's versions move by hand.** `demo/users` depends on this
  package by path and on `varyk-sql` and `varyk-std` by version; the
  change that moves the package to a new varyk moves the demo's lines
  too. Do not give release-please an `extra-files` entry for the demo:
  it would write this package's version into the demo's `varyk-std` line.
- **Docs move with the code.** README.md, CONTRIBUTING.md,
  `docs/specs/`, and the demo's comments are updated in the same change
  whenever what they describe changes: status, versions, install,
  usage, behavior, defaults, features, links. Check them against the
  change before every pull request.
