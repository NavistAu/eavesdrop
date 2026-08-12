//! eavesdrop — a read-only tail of cross-session messages between Claude Code
//! agents.
//!
//! Claude Code sessions can message each other, but nothing shows that traffic:
//! you can see that a peer is idle or wedged, never what anyone said to whom.
//! This reads it back out of the session transcripts both ends already write.
//!
//! **Read-only, always.** It never sends, never marks a message read, never
//! touches a session. If it ever grows a way to reply, that is a different
//! project.

mod extract;
mod feed;
mod registry;
mod render;
mod ui;
mod watch;

use clap::Parser;
use feed::Feed;
use registry::Registry;
use render::Groups;
use std::collections::HashSet;
use std::io::{IsTerminal, Write};
use std::time::{Duration, SystemTime};
use watch::Watcher;

/// How often tracked files are checked for new bytes.
const POLL: Duration = Duration::from_millis(400);
/// How long a message waits before printing, so the other half of the exchange
/// can be folded in first.
///
/// The two records' *timestamps* are ~60 ms apart, but that is not what this
/// has to cover: what matters is when each reaches disk. The recipient's
/// enqueue is written immediately, while the sender's record is not complete
/// until its tool_result lands — 558 ms later on one measured send — and each
/// side is then found up to one `POLL` late. A first attempt at 750 ms split a
/// real exchange across two rows. `Feed` drops a late second half rather than
/// reprinting it, so this only needs to cover the common case.
const GRACE_MS: i64 = 2500;

#[derive(Parser)]
#[command(
    name = "eavesdrop",
    version,
    about = "Tail cross-session messages between Claude Code agents (read-only)"
)]
struct Cli {
    /// Only show agents running in panes of the current tmux window
    #[arg(long)]
    pane: bool,

    /// How much history to read first, e.g. 90s, 30m, 24h, 7d
    #[arg(long, default_value = "24h", value_name = "DURATION")]
    since: String,

    /// Show at most this many historical messages
    #[arg(long, value_name = "N")]
    tail: Option<usize>,

    /// Print history and exit instead of following
    #[arg(long)]
    no_follow: bool,

    /// Emit one JSON object per message instead of formatted text
    #[arg(long)]
    json: bool,
}

fn main() {
    let cli = Cli::parse();
    if let Err(message) = run(cli) {
        eprintln!("eavesdrop: {message}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<(), String> {
    let window = parse_duration(&cli.since)
        .ok_or_else(|| format!("cannot read --since {:?}; try 30m, 24h, 7d", cli.since))?;
    let cutoff_wall = SystemTime::now() - window;
    let cutoff = chrono::Utc::now() - chrono::Duration::from_std(window).unwrap_or_default();

    let projects = registry::config_dir().join("projects");
    if !projects.is_dir() {
        return Err(format!("no transcripts at {}", projects.display()));
    }

    let registry = Registry::load();
    // Named here, while the registry is still owned, so an empty feed can say
    // exactly which agents it scoped to. `--pane` finding nothing is a normal
    // outcome — most windows hold agents that never message each other — and
    // it is indistinguishable from a broken filter unless the tool says so.
    let mut watching: Vec<String> = Vec::new();
    let filter = if cli.pane {
        let panes = registry
            .current_window_panes()
            .ok_or("--pane needs to run inside tmux; $TMUX_PANE is unset or names no known pane")?;
        let sessions = registry.sessions_in_panes(&panes);
        if sessions.is_empty() {
            return Err(format!(
                "no Claude sessions in this tmux window ({} panes checked)",
                panes.len()
            ));
        }
        watching = sessions.iter().map(|id| registry.label_of(id)).collect();
        watching.sort();
        Some(sessions.into_iter().collect::<HashSet<_>>())
    } else {
        None
    };

    let mut feed = Feed::new(registry);
    let mut watcher = Watcher::new(projects);
    let mut observations = Vec::new();
    let scanned = watcher.scan_initial(cutoff_wall, &mut observations);
    feed.ingest(std::mem::take(&mut observations));

    let scope = if cli.pane {
        "this tmux window"
    } else {
        "all sessions"
    };
    let banner = format!("{scanned} transcripts in the last {}, {scope}", cli.since);

    // What to show when the feed is legitimately empty. Naming the agents it
    // scoped to is the whole point: `--pane` finding nothing is normal, and
    // without this it is indistinguishable from a filter that is broken.
    let mut empty_hint = vec![format!("No messages in the last {}.", cli.since)];
    if cli.pane {
        empty_hint.push(String::new());
        empty_hint.push(format!(
            "Watching {} agent{} in this tmux window:",
            watching.len(),
            if watching.len() == 1 { "" } else { "s" }
        ));
        empty_hint.push(format!("  {}", watching.join(", ")));
        empty_hint.push(String::new());
        empty_hint.push(
            "None of them has messaged another session in this window. Agents in other".into(),
        );
        empty_hint.push("windows are excluded — run without --pane to watch every window.".into());
    } else {
        empty_hint.push(String::new());
        empty_hint.push("Nothing has been sent between sessions yet. Try a longer --since.".into());
    }

    // History is flushed with no grace: every record is already on disk, so
    // there is nothing still in flight to wait for.
    let mut history: Vec<_> = feed
        .take_ready(chrono::Utc::now(), 0)
        .into_iter()
        .filter(|m| m.ts >= cutoff)
        .filter(|m| passes(&feed, m, filter.as_ref()))
        .collect();
    let mut printed: HashSet<String> = HashSet::new();
    if let Some(limit) = cli.tail {
        let drop = history.len().saturating_sub(limit);
        for message in history.drain(..drop) {
            printed.insert(message.key());
        }
    }
    for message in &history {
        printed.insert(message.key());
    }

    // The interactive view is for a terminal that is actually being watched.
    // Piped output, `--json` and `--no-follow` all want a plain stream, and
    // taking over the screen for any of them would be actively wrong.
    let interactive = !cli.json && !cli.no_follow && std::io::stdout().is_terminal();
    let start = Started {
        history,
        printed,
        filter,
        cutoff,
        banner,
        empty_hint,
    };
    if interactive {
        return follow_interactive(feed, watcher, start);
    }
    follow_plain(cli, feed, watcher, start)
}

/// What both follow paths need once the startup scan is done: the history to
/// show, what has already been accounted for, and the filters to keep applying.
struct Started {
    history: Vec<feed::Message>,
    printed: HashSet<String>,
    filter: Option<HashSet<String>>,
    cutoff: chrono::DateTime<chrono::Utc>,
    banner: String,
    /// Shown instead of an empty feed, naming what was actually searched.
    empty_hint: Vec<String>,
}

/// The scrollback view: hold the messages, redraw, re-wrap on resize.
fn follow_interactive(feed: Feed, mut watcher: Watcher, start: Started) -> Result<(), String> {
    let Started {
        history,
        mut printed,
        filter,
        cutoff,
        banner,
        empty_hint,
    } = start;
    let mut app = ui::App::new(feed, banner, empty_hint);
    for message in history {
        app.push(message);
    }

    let mut screen = ui::Screen::open().map_err(|e| format!("cannot start the display: {e}"))?;
    let mut observations = Vec::new();
    let mut last_poll = SystemTime::now();
    loop {
        if !ui::tick(&mut screen, &mut app).map_err(|e| format!("display failed: {e}"))? {
            return Ok(());
        }
        if last_poll.elapsed().unwrap_or_default() < POLL {
            continue;
        }
        last_poll = SystemTime::now();
        watcher.poll(&mut observations);
        if !observations.is_empty() {
            app.feed.ingest(std::mem::take(&mut observations));
        }
        for message in app.feed.take_ready(chrono::Utc::now(), GRACE_MS) {
            if message.ts < cutoff || !printed.insert(message.key()) {
                continue;
            }
            if !passes(&app.feed, &message, filter.as_ref()) {
                continue;
            }
            app.push(message);
        }
    }
}

/// The plain stream: one message after another to stdout, never redrawn.
fn follow_plain(
    cli: Cli,
    mut feed: Feed,
    mut watcher: Watcher,
    start: Started,
) -> Result<(), String> {
    let Started {
        history,
        mut printed,
        filter,
        cutoff,
        banner,
        empty_hint,
    } = start;
    let is_tty = std::io::stdout().is_terminal();
    let colour_on = !cli.json && is_tty && std::env::var_os("NO_COLOR").is_none();
    // Zero means "do not wrap": piped output belongs to whatever consumes it.
    let width = if cli.json || !is_tty {
        0
    } else {
        terminal_width()
    };
    let mut groups = Groups::new();
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());

    if !cli.json {
        eprintln!(
            "eavesdrop: {banner}{}",
            if cli.no_follow { "" } else { ", following" }
        );
        // Same reasoning as the interactive empty state: silence after the
        // banner is indistinguishable from a filter that matched nothing by
        // mistake. On stderr, so a pipeline still sees an empty stdout.
        if history.is_empty() {
            for line in &empty_hint {
                eprintln!("  {line}");
            }
        }
    }

    for message in &history {
        if emit(
            &mut out,
            &feed,
            &mut groups,
            message,
            colour_on,
            width,
            cli.json,
        )
        .is_err()
        {
            return Ok(());
        }
    }
    if out.flush().is_err() || cli.no_follow {
        return Ok(());
    }

    let mut observations = Vec::new();
    loop {
        std::thread::sleep(POLL);
        watcher.poll(&mut observations);
        if !observations.is_empty() {
            feed.ingest(std::mem::take(&mut observations));
        }

        let mut wrote = false;
        for message in feed.take_ready(chrono::Utc::now(), GRACE_MS) {
            if message.ts < cutoff || !printed.insert(message.key()) {
                continue;
            }
            if !passes(&feed, &message, filter.as_ref()) {
                continue;
            }
            if emit(
                &mut out,
                &feed,
                &mut groups,
                &message,
                colour_on,
                width,
                cli.json,
            )
            .is_err()
            {
                return Ok(());
            }
            wrote = true;
        }
        if wrote && out.flush().is_err() {
            return Ok(());
        }
    }
}

/// Keep a message when either end is a session in the pane filter. An endpoint
/// that could not be resolved to a session id never matches, so `--pane` is
/// strict: it shows traffic it can prove belongs to this window.
fn passes(feed: &Feed, message: &feed::Message, filter: Option<&HashSet<String>>) -> bool {
    let Some(filter) = filter else { return true };
    let (from, to) = feed.endpoints(message);
    filter.contains(&from) || filter.contains(&to)
}

fn emit(
    out: &mut impl Write,
    feed: &Feed,
    groups: &mut Groups,
    message: &feed::Message,
    colour_on: bool,
    width: usize,
    as_json: bool,
) -> std::io::Result<()> {
    if as_json {
        return writeln!(out, "{}", render::json(feed, message));
    }
    for row in render::message_rows(feed, groups, message, width) {
        writeln!(out, "{}", render::row_to_ansi(&row, colour_on))?;
    }
    Ok(())
}

/// Columns available for output, or a sane default.
///
/// Asked once at startup via `tput`, rather than taking a dependency for one
/// number. A resize mid-follow is not tracked; the width only decides where
/// paragraphs fold, so a stale value degrades to text narrower or wider than
/// ideal, never to text that is wrong.
fn terminal_width() -> usize {
    std::process::Command::new("tput")
        .arg("cols")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| {
            String::from_utf8_lossy(&out.stdout)
                .trim()
                .parse::<usize>()
                .ok()
        })
        .filter(|cols| *cols > 0)
        .unwrap_or(100)
}

/// Parse `90s`, `30m`, `24h`, `7d`. A bare number is seconds.
fn parse_duration(raw: &str) -> Option<Duration> {
    let raw = raw.trim();
    let (digits, multiplier) = match raw.chars().last()? {
        's' => (&raw[..raw.len() - 1], 1),
        'm' => (&raw[..raw.len() - 1], 60),
        'h' => (&raw[..raw.len() - 1], 3600),
        'd' => (&raw[..raw.len() - 1], 86400),
        _ => (raw, 1),
    };
    let value: u64 = digits.trim().parse().ok()?;
    Some(Duration::from_secs(value.checked_mul(multiplier)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_parse_with_and_without_a_unit() {
        assert_eq!(parse_duration("90s"), Some(Duration::from_secs(90)));
        assert_eq!(parse_duration("30m"), Some(Duration::from_secs(1800)));
        assert_eq!(parse_duration("24h"), Some(Duration::from_secs(86400)));
        assert_eq!(parse_duration("7d"), Some(Duration::from_secs(604800)));
        assert_eq!(parse_duration("45"), Some(Duration::from_secs(45)));
        assert_eq!(parse_duration(" 24h "), Some(Duration::from_secs(86400)));
    }

    #[test]
    fn nonsense_durations_are_rejected_rather_than_guessed() {
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration("h"), None);
        assert_eq!(parse_duration("-5m"), None);
        assert_eq!(parse_duration("2 weeks"), None);
        assert_eq!(parse_duration("99999999999999999999d"), None);
    }
}
