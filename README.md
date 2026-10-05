# varyk-http

The official HTTP package for [Varyk](https://varyk.com), a language
for backend services that compiles to Rust: an HTTP server and an HTTP
client for Varyk services.

varyk-http is not released yet. It is milestone 5b4 of Varyk's
[roadmap](https://github.com/Varyk-Lang/varyk/blob/main/docs/roadmap.md):
a server with an explicit route table, handlers whose parameters are
bound by name to the route and by type to the JSON body and to shared
state, with each route checked against its handler by `varyk check`;
errors that carry a status, where an error without one is a 500 whose
message is logged and never sent; and a client in the same package.
Until the first release lands, this repository holds the project's
conventions and nothing here builds. Varyk is experimental and pre-1.0:
anything here may change.

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
