# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Any change to what a transcript record is read as — dedupe rules, the
send/receive pairing logic, or which record types are extracted — is called
out here regardless of size, since those are the changes most likely to
silently change what shows up in the feed or drop something that should.

## [Unreleased]

## [0.1.0] - 2026-08-19

First public release.

### Added

- `eavesdrop`: a read-only tail of cross-session messages between Claude Code
  agents, read back out of session transcripts.
- History plus live follow by default: prints the last 24 hours, then
  follows. `--since`, `--tail`, and `--no-follow` control the history window.
- `--pane`: scope to the agents running in panes of the current tmux window,
  resolved through the session registry.
- `--json`: one JSON object per message, for piping.
- An interactive terminal view that reflows on resize, with scroll/follow
  keybindings and per-conversation colour grouping.
- An empty feed names what it scoped to instead of rendering a blank screen.
