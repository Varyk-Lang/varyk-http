# Working on varyk-http

This file is for anyone, human or AI agent, making changes to this
repository. [CONTRIBUTING.md](CONTRIBUTING.md) has the details; this is
the short list of rules.

## What this is

varyk-http is the official HTTP package for Varyk: an HTTP server and an
HTTP client. It is a Varyk package: `src/lib.vr` is everything a program
sees, the `.rs` files beside it are the facade over the Rust HTTP crates
and the only Rust, and `src/tests.vr` holds the tests. The design goes in
`docs/specs/` and is written before the code; milestone 5b4 of Varyk's
[roadmap](https://github.com/Varyk-Lang/varyk/blob/main/docs/roadmap.md)
is the brief. Read the spec before changing what the package offers.

## The gate

Run the checks CONTRIBUTING.md lists ("Build and test") before every
pull request: `varyk check`, `varyk test`, rustfmt on the `.rs` files,
and clippy on the crate `varyk publish --assemble-only` writes. CI's
`test` and `msrv` jobs are required checks: keep those job names, and
keep the code building on Rust 1.85 (no let-chains).

## Rules

- **No crash on absence.** The facade never panics: no `unwrap`,
  `expect`, or indexing that can fail. Every failure is a
  `varyk_std::Error`.
- **No internal failure reaches a client.** An error without a status
  is a 500: its message is logged and not sent. A response never carries
  a secret, a backtrace, or the text of a dependency's error.
- **Safe by default.** The package faces the network. Where the easy way
  and the safe way differ, the safe way is the default and the other is
  asked for by name.
- **Commits.** Conventional prefixes (`feat:`, `fix:`, `docs:`, ...),
  no `<` or `>` in subjects or `BREAKING CHANGE:` footers, and no
  attribution lines (no `Co-Authored-By`, no "Generated with").
- **Docs move with the code.** README.md, CONTRIBUTING.md, and
  `docs/specs/` are updated in the same change whenever what they
  describe changes: status, versions, install, usage, behavior,
  features, links. Check them against the change before every pull
  request.
