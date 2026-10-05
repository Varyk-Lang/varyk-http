# Contributing

varyk-http is the official HTTP package for Varyk. Varyk is experimental
and pre-1.0: anything may change, and a new feature or a breaking change
bumps the minor version. Contributions are welcome; small, focused pull
requests are the easiest to review. The project has one maintainer, so
reviews are best effort and a pull request may wait a while; a reminder
after two weeks is welcome.

The package is not released yet: milestone 5b4 of Varyk's
[roadmap](https://github.com/Varyk-Lang/varyk/blob/main/docs/roadmap.md)
is the brief, and the design spec is written before the code. Until the
code lands, the most useful contribution is a comment on that design,
as an issue here or in Varyk's
[Discussions](https://github.com/Varyk-Lang/varyk/discussions).

## Build and test

You need a stable Rust toolchain through [rustup](https://rustup.rs) and
`varyk` (`cargo install varyk --locked`). The package is a Varyk package
(`src/lib.vr`), so it is built only by `varyk`; plain `cargo build` does
not work here. Before opening a pull request, run the same checks CI
runs:

```sh
varyk check
varyk test
rustfmt --edition 2024 --check src/*.rs
cd "$(varyk publish --assemble-only)"
cargo clippy --all-targets -- -D warnings
```

`varyk publish --assemble-only` writes the plain Rust crate the package
publishes as, and prints its folder; clippy runs there. Once there is
code, `.github/workflows/ci.yml` has the exact steps and the varyk
version CI installs; it arrives with the code, and until then the `test`
and `msrv` checks that `main` requires have nothing to run.

The minimum supported Rust version is 1.85 and CI checks it: no
let-chains or other later features.

## Where things are

- `src/lib.vr` is everything a program sees; the `.rs` files beside it,
  the facade over the Rust HTTP crates, are the only Rust. They never
  panic: no `unwrap`, `expect`, or indexing that can fail, and every
  failure becomes a `varyk_std::Error`. An error without a status is a
  500 whose message is logged and not sent, so no internal failure
  reaches a client.
- `src/tests.vr` holds the tests, as a module: a Varyk package may not
  have a `tests/`, `examples/`, or `benches/` directory.
- `docs/specs/` holds the design; read it before changing what the
  package offers.

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
