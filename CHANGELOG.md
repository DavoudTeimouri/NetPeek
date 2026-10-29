# Changelog

All notable changes to this project are documented here.

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
versioning follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0] - 2026-10-29

### Added

- `netpeek http <url>`: HTTP status, DNS/connect/TLS/TTFB timing breakdown, TLS
  issuer, certificate expiry and SANs.
- `netpeek svc <name> [-n ns]`: resolves a Service to its ClusterIP and endpoint
  slices, then probes every backend and reports ready/not-ready state with the
  connect result.
- `serde_json` dependency, used only for parsing Kubernetes API bodies.

### Changed

- Container image is also tagged `latest` on the default branch.

## [0.1.0] - 2026-10-29

### Added

- `netpeek info`: interfaces, MTU, link state, routes, `/etc/resolv.conf` and
  listening TCP sockets.
- `netpeek dns <name>`: resolution result, addresses, round-trip time, resolvers
  and search domains.
- `netpeek tcp <host:port>`: connect test with latency and failure class.
- `--json`, `--timeout <secs>` and exit codes `0`/`1`/`2`.
- Multi-arch container image (`amd64`, `arm64`) bundling the standard net tools.
- GitHub Actions workflow: test, build, multi-arch image push to ghcr.io.

[Unreleased]: https://github.com/DavoudTeimouri/NetPeek/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/DavoudTeimouri/NetPeek/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/DavoudTeimouri/NetPeek/releases/tag/v0.1.0
