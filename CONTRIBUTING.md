# Contributing

varyk-http is the official HTTP package for Varyk. Varyk is experimental
and pre-1.0: anything may change, and a new feature or a breaking change
bumps the minor version. Contributions are welcome; small, focused pull
requests are the easiest to review. The project has one maintainer, so
reviews are best effort and a pull request may wait a while; a reminder
after two weeks is welcome.

The compiler side of the design is Varyk's
[5b4 spec](https://github.com/Varyk-Lang/varyk/blob/main/docs/specs/2026-10-05-milestone-5b4-design.md),
whose section 6 is the contract this package implements; the package
side is in `docs/specs/` here. A change to what the package offers
starts as an issue, here or in Varyk's
[Discussions](https://github.com/Varyk-Lang/varyk/discussions).

## Build and test

You need a stable Rust toolchain through [rustup](https://rustup.rs), a
C compiler (the demo's SQLite is built from C), and `varyk` (`cargo
install varyk --version '^0.7' --locked`, the latest 0.7 release, as CI
uses). The package is a Varyk package
(`src/lib.vr`), so it is built only by `varyk`; plain `cargo build`
does not work here. Before opening a pull request, run the same checks
CI runs:

```sh
varyk check
varyk test
(cd demo/users && varyk test)
rustfmt --edition 2024 --check src/*.rs
cd "$(varyk publish --assemble-only)"
cargo clippy --all-targets -- -D warnings
```

`varyk publish --assemble-only` writes the plain Rust crate the package
publishes as, and prints its folder; clippy runs there.
`.github/workflows/ci.yml` has the exact steps.

The minimum supported Rust version is 1.85 and CI checks it: no
let-chains or other later features.

Every Monday, and on demand from the Actions tab, the "Latest varyk"
workflow builds and tests varyk-http with the newest varyk on crates.io,
moving `varyk-std` to its version for that run. It is not a required
check; a red run means a new varyk needs a varyk-http release.

## Where things are

- `src/lib.vr` is everything a program sees: the `pub use` list and the
  error constructors, in Varyk. The `.rs` files beside it are the facade
  over axum, tower-http, and reqwest, and the only Rust: `server.rs`
  (`App`, its settings, the router, `serve`, `request`, the route
  wrapper, the hooks), `message.rs` (`Request`, `Response`, cookies,
  files, the `respond_*` functions), `live.rs` (`WebSocket`, `Sse`,
  `Multipart`, `Part`), `client.rs` (`Client`), and `metrics.rs`. They
  never panic: no `unwrap`, `expect`, or indexing that can fail, and
  every failure becomes a `varyk_std::Error`. An error without a status
  is a 500 whose message is logged and not sent, so no internal failure
  reaches a client. The items the compiler's generated Rust calls
  (section 6 of the 5b4 spec) change only with a varyk release.
- `src/tests.vr` holds the tests (the Rust tests, for what a Varyk
  program cannot reach, are in the facade's `#[cfg(test)]` modules,
  which `varyk test` also runs: a WebSocket conversation with
  tokio-tungstenite as the client in `src/live.rs`, the logged path in
  `src/message.rs`, and the idle timeout in `src/server.rs`),
  as a module: a Varyk package may not have a `tests/`, `examples/`, or
  `benches/` directory. Its own module `src/tests/handlers.vr` holds a
  handler of another module, for a route that names one. Most send
  requests through `app.request`, with no port; the event-stream,
  client, in-flight, WebSocket, and idle-timeout tests serve on a fixed
  loopback port, from 41801 up, one port per test, so tests running at once never
  share one. The file tests serve from `testdata/` (a text file, and a
  link pointing out of the folder that must stay refused), and the
  upload tests write under the git-ignored `target/`, so run `varyk
  test` from the package's folder, as CI does.
- `demo/users` is the users API of the README's first service, on
  `varyk-sql` and SQLite, with its tests in `src/tests.vr` through
  `app.request`; CI runs its `varyk test`. It depends on this package
  by path and on `varyk-sql` by version, the `varyk-sql` release for
  the same varyk; a change that moves the package to a new varyk moves
  the demo's `varyk-std` and `varyk-sql` lines with it. release-please
  does not touch the demo's manifest.
- `docs/specs/` holds the package's design, and `docs/plans/` the plan
  it was built from; read the spec, and the compiler's 5b4 spec, before
  changing what the package offers.

Rust code follows Varyk's
[AGENTS.md](https://github.com/Varyk-Lang/varyk/blob/main/AGENTS.md):
the smallest change that works, failures with plain-word messages, and
safe by default.

## Commit messages and releases

Releases are cut by
[release-please](https://github.com/googleapis/release-please) from the
commit history on `main`, so every commit that lands on `main` carries a
conventional prefix. A pull request with one concern is squash-merged
with a conventional title; a pull request with several concerns is
rebase-merged with one conventional commit per concern.

| Prefix | Use it for | Effect on the release |
|---|---|---|
| `feat:` | a new capability | bumps the version, listed in the changelog |
| `fix:` | a bug fix | bumps the version, listed in the changelog |
| `docs:` | documentation only | no bump, not listed |
| `test:` | tests only | no bump, not listed |
| `ci:` | workflows and release automation | no bump, not listed |
| `chore:` | maintenance, dependencies, manifests | no bump, not listed |
| `refactor:` | code change with no behavior change | no bump, not listed |

A `!` after the prefix (`feat!:`) marks a breaking change. Before 1.0,
`feat` and a breaking change bump the minor version and `fix` bumps the
patch version.

Keep `<` and `>` out of commit subjects and `BREAKING CHANGE:` footers:
write `Option of T`, not `Option<T>`. release-please copies them into
the release pull request and reads that body as HTML, where an unclosed
`<T>` hides what follows it. The `Commit messages` check enforces this
on every pull request.

release-please opens a release pull request on `main` with the version
bump and `CHANGELOG.md`. Merging that pull request is the release
decision: it creates the tag `vX.Y.Z` and the GitHub release, and the
publish job then uploads the crate to crates.io with no further
approval. The crates.io token is a secret of the `release` environment,
which only workflow runs from protected branches (`main`) may use.

## Licensing of contributions

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in varyk-http by you, as defined in the
Apache-2.0 license, shall be dual licensed under MIT or Apache-2.0,
without any additional terms or conditions. The name "Varyk" is a
trademark and is not covered by those licenses; see Varyk's
[TRADEMARKS.md](https://github.com/Varyk-Lang/varyk/blob/main/TRADEMARKS.md).

## Reporting bugs

Open an issue with the smallest Varyk program that shows the problem,
the request that triggers it (a `curl` line is ideal), and the response
or error message. For security problems, see the security policy shared
by every Varyk-Lang repository:
<https://github.com/Varyk-Lang/varyk-http/security/policy>.
