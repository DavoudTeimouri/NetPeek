# Contributing

## Ground rules

- One check per pull request. A check is a diagnostic question, not a wrapper
  around a tool that already exists in the image.
- Zero new runtime dependencies unless the alternative is hand-rolling something
  with a bug surface (JSON parsing qualifies; a URL splitter does not).
- Every non-trivial parser gets an assert-based test. No test frameworks.
- New command output must look right in both text and `--json`.

## Setup

```bash
cargo test                     # assert-based checks
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo build --release
./target/release/netpeek info
```

No Docker needed to develop. The image only matters for the bundled tools; if
`ip` is missing locally, `netpeek info` says so instead of failing.

To verify inside a cluster:

```bash
docker build -t netpeek:dev .
kind load docker-image netpeek:dev          # or push to your registry
kubectl run netpeek --rm -it --restart=Never --image=netpeek:dev -- netpeek info
```

## Commit messages

`<scope>: <what changed>`, imperative, lower case after the colon. Body explains
why, not what. Examples:

- `svc: probe endpoint slices instead of the legacy endpoints object`
- `http: report cert SANs so hostname mismatches are obvious`

## Before opening a pull request

- `cargo test`, `cargo clippy`, `cargo fmt --check` are clean.
- You ran the command against something real and pasted the output in the PR.
- If the behaviour change is user-visible, `CHANGELOG.md` has an entry under
  `Unreleased`.
