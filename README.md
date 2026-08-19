# eavesdrop

[![CI](https://github.com/navistau/eavesdrop/actions/workflows/ci.yml/badge.svg)](https://github.com/navistau/eavesdrop/actions/workflows/ci.yml)
[![License: AGPL-3.0](https://img.shields.io/badge/License-AGPL--3.0-blue.svg)](LICENSE)

A read-only tail of the messages Claude Code sessions send each other.

Sessions can message each other with `SendMessage`, but nothing shows that
traffic. You can see *that* a peer is idle or wedged; you cannot see what
anyone said to whom. With one agent supervising several others, the
coordination is invisible.

`eavesdrop` reads it back out of the session transcripts both ends already
write. It never sends, never marks a message read, never touches a session.

```
$ eavesdrop
eavesdrop: 26 transcripts in the last 24h, all sessions, following
00:29:51        eavesdrop-5c → targetagent
        Please send me one short message — I want to confirm the round trip works.
00:30:01         targetagent → eavesdrop-5c
        Round trip confirmed from the tmux-bridge session. For your record: you
        never appeared in my ListAgents under the old name either.
```

Each conversation gets its own colour. A conversation is a connected group of
agents talking — a supervisor and its four workers is one colour, not five —
so an interleaved feed stays readable when several are running at once.

In a terminal it runs as a scrollback you can move around in, and it re-wraps
every message when the window resizes. Piped, or with `--json` or
`--no-follow`, it is a plain stream instead.

## Requirements

- Rust 1.95 or later, to build from source.
- Claude Code 2.1.224 or later — the version that added `SendMessage` and
  `ListAgents`.

## Install

Homebrew (macOS):

```sh
brew install navistau/tap/eavesdrop
```

Or grab a prebuilt binary from the [releases page](https://github.com/navistau/eavesdrop/releases) —
one `.tar.gz` per platform, each with a `.sha256` to verify it against.

Or clone the repo and build from source:

```sh
mise run release
```

Builds in release mode and installs the binary to `~/.local/bin/eavesdrop`.
The install is a mise task rather than part of the build because cargo has no
post-build hook — `build.rs` runs before the binary is linked.

Needs Rust 1.95 (`mise install` picks it up from `mise.toml`).


## Use

No arguments watches everything: it reads the last 24 hours of history, prints
it, then follows live.

| Flag | Effect |
|---|---|
| `--pane` | Only agents running in panes of the current tmux window |
| `--since 30m` | How much history to read first (`90s`, `30m`, `24h`, `7d`) |
| `--tail N` | Show at most N historical messages |
| `--no-follow` | Print history and exit |
| `--json` | One JSON object per message, for piping |

In the interactive view: `↑`/`↓` or `j`/`k` scroll a line, `PgUp`/`PgDn` a page,
`g`/`Home` jumps to the start, `G`/`End` returns to following the tail, and `q`
or `Esc` quits. Scrolling up detaches from the tail; `G` reattaches.

Mouse capture is deliberately off, so click-drag selection still works — being
able to copy what an agent said matters more than wheel scrolling.

`--pane` answers "what are the agents *in front of me* saying to each other".
It maps the window's panes to sessions through the session registry, so it only
shows traffic it can prove belongs to this window — and it scopes to the window
holding the pane it runs in, not whichever window you happen to be looking at.

The set of agents is re-read every 5s, so one that starts (or restarts, taking
a new session id) while eavesdrop is running is picked up. Messages it sent
*before* it registered stay hidden — widening the filter is not retroactive.

Finding nothing is a normal result: most windows hold agents that never message
each other. Rather than a blank screen it names what it scoped to, so an empty
feed can be told apart from a filter that missed:

```
No messages in the last 60s.

Watching 5 agents in this tmux window:
  eavesdrop-5c, targetagent, targetagent2, tmux-bridge-86, tmux-bridge-d9

None of them has messaged another session in this window. Agents in other
windows are excluded — run without --pane to watch every window.
```

Colour is suppressed when stdout is not a terminal, or when `NO_COLOR` is set.

## How it works

Transcripts live at `<config_dir>/projects/<cwd-slug>/<session-id>.jsonl`,
where `config_dir` is `$CLAUDE_CONFIG_DIR` or `~/.claude`.

Reading transcripts rather than hooking `SendMessage` is deliberate. A
`PreToolUse` hook sees only outgoing messages, only from sessions that have the
hook installed, and nothing that already happened. Transcripts need no
cooperation from any session, catch both directions, and work retroactively.

One exchange is written to two files:

- The **sender** records a `SendMessage` tool_use *and* its tool_result. The
  result is what makes it real — `success` and a `msg_id`. A tool_use alone
  proves nothing: the first send observed during development failed on an
  ambiguous name and had to be re-sent with a `[ref]` suffix. Unconfirmed sends
  are not shown.
- The **recipient** records a `queue-operation` enqueue carrying a
  `<cross-session-message>` envelope. It comes in two shapes: a send addressed
  by name arrives as `from-name="<title>"`, while one agent replying to another
  arrives as `from="uds:<socket>"` with a `hop-chain`. A `uds:` address resolves
  back to a session through the registry's `messagingSocketPath`.

Neither side carries the other's session id — the sender addresses a registry
*name*, the recipient is told a conversation *title* — so the two records are
paired on the message body, which is byte-identical across them. That pairing
is what lets a row name both ends even when one of them has since exited. A row
seen from only one side says so rather than pretending: `(no delivery record)`
means the recipient's transcript fell outside the window, `(no sender record)`
the sender's did.

Bounding the read is the load-bearing constraint: a working machine had 1,999
transcripts totalling 2.5 GB. The mtime filter cuts a 24-hour window to 26
files and 121 MB, and each line is substring-filtered before it reaches the
JSON parser. A 7-day window over 678 files runs in 0.8 s.

### Deliberate limits

- **Extraction discipline.** Transcripts contain everything a session ever did.
  This reads message records and session titles, nothing else. It must not
  drift into being a general transcript reader — that is a different tool with
  different privacy properties.
- **Colour is fixed once printed — in the plain stream only.** If two groups
  later merge, the older group's colour survives and new rows join it, but the
  absorbed group's earlier rows keep their original colour, because a stream
  cannot repaint what has scrolled. The interactive view re-lays out every
  message on each draw, so there a merge repaints the whole group.
- **Messages print in full, and the sender's `summary` is never shown.** This is
  a log of what agents said, so the words are the content and everything else is
  addressing. A summary reads like a tidier label, but only the *sending* side
  records one — showing it made the text mean different things depending on
  which transcript happened to be scanned, and hid the message behind a label.
  `--json` still carries `summary`, where it is labelled.
- **A late second half is dropped, not re-rendered.** While following, the two
  records reach disk hundreds of milliseconds apart — the recipient's enqueue
  is written immediately, the sender's only once its tool_result lands. Rows
  wait 2.5 s so the common case pairs; if the other half turns up after that,
  the row keeps its one-sided caveat rather than printing again.
- **Only the enqueue record is read** on the receiving side. Claude Code also
  writes the same envelope to a `user` record on delivery, at a slightly
  different timestamp, so the two cannot be deduped on time. Ignoring the
  second one also avoids matching any session that merely *discusses*
  cross-session messaging in prose.
- **Labels degrade.** An agent is named by what you called it first: the
  `tb mark` codeword (`@tb_name`), then the pane tag (`@pane_tag`), then the
  registry name agents use to address each other. After a session exits none of
  those exist, so the name is reconstructed from its working directory, then its
  conversation title, then a short session id.

### Unverified

- Whether a **held** message (`crossSessionInbound: hold`) is written to the
  transcript at all, or only on delivery. Worth knowing — held messages are
  exactly the ones an operator wants to see.
- Whether **refused** messages leave any trace.
- Cross-machine messages via Remote Control.

## Roadmap

- **Supervisor↔subagent traffic.** `SendMessage` addressed to an in-session
  agent id resumes that agent, and its reply comes back as a
  `<task-notification>` with a `<result>` block. Different mechanism, same
  conversational shape, and far more of it — 248 such sends against 3
  cross-session ones over six days on the development machine. It would need
  its own flag so the default feed stays readable.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for the branch model, local build and
test steps, and documentation style. This project follows the
[Contributor Covenant](CODE_OF_CONDUCT.md).

## Security

See [SECURITY.md](SECURITY.md) to report a vulnerability privately.

## License

[AGPL-3.0-only](LICENSE). It reads a session's transcripts; the network-use
clause keeps hosted variants of this tool open-source too.
