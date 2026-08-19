# Contributing

Outside contributions are welcome. This document covers the branch model, how to build and test locally, the documentation style this repo uses, and what bar a new flag needs to clear.

## Branch model

- Feature branches (`feat/…`, `fix/…`, `docs/…`) start from `develop` and target `develop` in their pull request.
- `develop` → `main` is the only path to `main`.
- Every push to `main` is a release: CI builds and publishes binaries for the version in `Cargo.toml`. Don't open a PR directly against `main`.

If you don't have push access, fork the repo, branch from `develop` on your fork, and open your pull request back against this repo's `develop` branch.

If the `publish` job fails after it has already pushed the release tag, every subsequent push to `main` fails the `gate` job's tag-exists check by design — that check refuses to silently re-release a version. Recover by re-running the Release workflow via `workflow_dispatch` with `force: true`, which re-tags (force) and upserts the release rather than requiring a version bump.

## Build and test

```sh
mise install     # pins rust 1.95, see mise.toml
cargo build
cargo fmt --check
cargo clippy -- -D warnings
cargo test
```

`cargo test` runs both the unit tests and `tests/follow.rs`, which builds the real binary and drives it as a subprocess against synthetic transcript fixtures.

CI runs the same three checks (`cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test`) on every PR, so running them locally first catches failures before CI does.

## Documentation style: two registers

This README deliberately mixes two different registers, and a PR that touches docs needs to preserve the split rather than "fixing" it toward one style:

- **Controlled, terse language** in the Requirements, Installation, and flag-reference sections. These are reference material — a flag's default, an install step. Precision and scannability matter more than voice here.
- **Ordinary prose** everywhere else — the opening, "How it works," "Deliberate limits." These sections explain motivation and trade-offs, which reads better as normal writing than as a table.

If your PR touches README.md, match the register of the section you're editing. Don't convert prose sections into tables, and don't loosen the terse sections into narrative.

## Extraction discipline

`eavesdrop` reads only message and session-title records out of transcripts that can contain anything a session ever did. A PR that widens what gets read or parsed needs a clear reason tied to the message/receive shapes documented in the README — this must not drift into a general transcript reader, which is a different tool with different privacy properties.

## Adding configuration surface

New flags and options are not free — every one is a thing to document, test, and support indefinitely. Before adding one, have a concrete use case the existing surface can't already express. "Might be useful" isn't sufficient by itself. If you're not sure whether your use case clears this bar, open an issue describing it before writing the PR.

## CHANGELOG discipline

This project follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Add your entry to `CHANGELOG.md` as part of the same PR that makes the change — not deferred to release time. If your change affects what a transcript record is read as (dedupe rules, send/receive pairing, which record types are extracted), call that out explicitly regardless of how small it is.

## Code of conduct

This project follows the [Contributor Covenant](CODE_OF_CONDUCT.md). Reports go to `foss+conduct@navist.com.au`.
