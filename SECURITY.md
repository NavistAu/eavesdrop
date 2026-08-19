# Security Policy

`eavesdrop` reads Claude Code session transcripts, which can contain anything
a session did — treat any bug that widens what it reads, or that leaks
transcript content somewhere it shouldn't go, as a security issue.

## Supported versions

Only the latest release receives security fixes.

| Version | Supported |
| ------- | --------- |
| latest 0.x.y | :white_check_mark: |
| older 0.x.y  | :x: |

## Reporting a vulnerability

Please report security vulnerabilities privately via
[GitHub Security Advisories](https://github.com/navistau/eavesdrop/security/advisories/new)
for this repository — do not open a public issue.

We aim to acknowledge reports within **7 days**. Once a fix is available,
we'll coordinate disclosure timing with you.

## In scope

- The `eavesdrop` binary: transcript scanning, parsing, and rendering.
- Anything that could make it read outside the transcript directories it
  documents, or extract more than message and title records (see "Extraction
  discipline" in the README).

## Out of scope

- Claude Code itself — report those to Anthropic, not here.
- Vulnerabilities in dependencies — report upstream to the dependency's
  maintainers. A heads-up here is still welcome so this project can track and
  update, but the fix belongs upstream.
