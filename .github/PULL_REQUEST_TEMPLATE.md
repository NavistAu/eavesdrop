<!--
Feature branches target `develop`. Only the release PR targets `main`.
See CONTRIBUTING.md for the branch model.
-->

## What this changes

<!-- One or two sentences. What behaviour is different after this merge? -->

## Why

<!-- The problem this solves. Link the issue if there is one. -->

Closes #

## Checklist

- [ ] `cargo test` passes.
- [ ] `cargo fmt --check` reports nothing.
- [ ] `cargo clippy -- -D warnings` reports nothing.
- [ ] `CHANGELOG.md` has an entry under `## [Unreleased]`.
- [ ] Documentation is updated if a flag or record shape changed.

## Extraction discipline

<!--
Delete this section if the change does not touch what gets read from a
transcript. Otherwise: does this widen which record types are parsed, or
what fields are extracted from them? Say why, and confirm it still can't
read message bodies it isn't supposed to.
-->
