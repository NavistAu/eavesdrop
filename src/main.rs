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

use clap::Parser;
use extract::{Observation, Scanner};
use feed::Feed;
use registry::Registry;
use render::{Groups, Style};
use std::collections::{HashMap, HashSet};
use std::io::{ErrorKind, IsTerminal, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// How often tracked files are checked for new bytes.
const POLL: Duration = Duration::from_millis(400);
/// How often the whole projects tree is rescanned for files that have started
/// moving. Coarser than `POLL` because it stats every transcript on the
/// machine (~2,000 of them) rather than the handful being followed.
const REDISCOVER: Duration = Duration::from_secs(2);
/// How much of a newly-active transcript to read back. A file that starts
/// growing mid-run was not read at startup, so its triggering message is
/// already on disk; this recovers it without re-reading a multi-megabyte
/// history.
const TAIL_BACK: u64 = 256 * 1024;
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

    /// Print full message bodies, not just the summary line
    #[arg(long)]
    full: bool,

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
    let filter = if cli.pane {
        let panes = registry::current_window_panes()
            .ok_or("--pane needs to run inside tmux; $TMUX is unset or tmux is unavailable")?;
        let sessions = registry.sessions_in_panes(&panes);
        if sessions.is_empty() {
            return Err(format!(
                "no Claude sessions in this tmux window ({} panes checked)",
                panes.len()
            ));
        }
        Some(sessions.into_iter().collect::<HashSet<_>>())
    } else {
        None
    };

    let mut feed = Feed::new(registry);
    let mut scanner = Scanner::new();
    let mut tracked: HashMap<PathBuf, u64> = HashMap::new();
    let mut observations = Vec::new();

    // Startup: read every transcript touched inside the window, in full. The
    // mtime filter is what makes this affordable — a day's window is ~26 files
    // out of ~2,000, and ~120 MB out of ~2.5 GB.
    let mut scanned = 0usize;
    for (path, mtime, _) in discover(&projects) {
        if mtime < cutoff_wall {
            continue;
        }
        if let Some(offset) = read_from(&path, 0, &mut scanner, &mut observations) {
            tracked.insert(path, offset);
            scanned += 1;
        }
    }
    feed.ingest(std::mem::take(&mut observations));

    let style = Style {
        colour: !cli.json
            && std::io::stdout().is_terminal()
            && std::env::var_os("NO_COLOR").is_none(),
        full: cli.full,
    };
    let mut groups = Groups::new();
    let mut printed: HashSet<String> = HashSet::new();
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());

    if !cli.json {
        let scope = if cli.pane {
            "this tmux window"
        } else {
            "all sessions"
        };
        eprintln!(
            "eavesdrop: {scanned} transcripts in the last {}, {scope}{}",
            cli.since,
            if cli.no_follow { "" } else { ", following" }
        );
    }

    // History is flushed with no grace: every record is already on disk, so
    // there is nothing still in flight to wait for.
    let mut history: Vec<_> = feed
        .take_ready(chrono::Utc::now(), 0)
        .into_iter()
        .filter(|m| m.ts >= cutoff)
        .filter(|m| passes(&feed, m, filter.as_ref()))
        .collect();
    if let Some(limit) = cli.tail {
        let drop = history.len().saturating_sub(limit);
        for message in history.drain(..drop) {
            printed.insert(message.key());
        }
    }
    for message in &history {
        printed.insert(message.key());
        if emit(&mut out, &feed, &mut groups, message, style, cli.json).is_err() {
            return Ok(());
        }
    }
    if out.flush().is_err() {
        return Ok(());
    }

    if cli.no_follow {
        return Ok(());
    }

    let mut last_rediscover = SystemTime::now();
    loop {
        std::thread::sleep(POLL);

        if last_rediscover.elapsed().unwrap_or_default() >= REDISCOVER {
            last_rediscover = SystemTime::now();
            for (path, _, len) in discover(&projects) {
                tracked
                    .entry(path)
                    .or_insert_with(|| len.saturating_sub(TAIL_BACK));
            }
        }

        for (path, offset) in tracked.iter_mut() {
            let Ok(len) = std::fs::metadata(path).map(|m| m.len()) else {
                continue;
            };
            if len < *offset {
                // Truncated or replaced; start over rather than read garbage.
                *offset = 0;
            }
            if len == *offset {
                continue;
            }
            if let Some(new_offset) = read_from(path, *offset, &mut scanner, &mut observations) {
                *offset = new_offset;
            }
        }

        if !observations.is_empty() {
            feed.ingest(std::mem::take(&mut observations));
        }

        let ready = feed.take_ready(chrono::Utc::now(), GRACE_MS);
        let mut wrote = false;
        for message in ready {
            if message.ts < cutoff || !printed.insert(message.key()) {
                continue;
            }
            if !passes(&feed, &message, filter.as_ref()) {
                continue;
            }
            if emit(&mut out, &feed, &mut groups, &message, style, cli.json).is_err() {
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
    style: Style,
    as_json: bool,
) -> std::io::Result<()> {
    let text = if as_json {
        render::json(feed, message)
    } else {
        render::line(feed, groups, message, style)
    };
    writeln!(out, "{text}")
}

/// Every transcript under `projects`, with its mtime and length. Unreadable
/// directories and entries are skipped; there is no state to corrupt by
/// missing one.
fn discover(projects: &Path) -> Vec<(PathBuf, SystemTime, u64)> {
    let mut out = Vec::new();
    let Ok(dirs) = std::fs::read_dir(projects) else {
        return out;
    };
    for dir in dirs.flatten() {
        let Ok(files) = std::fs::read_dir(dir.path()) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(meta) = file.metadata() else { continue };
            let Ok(mtime) = meta.modified() else { continue };
            out.push((path, mtime, meta.len()));
        }
    }
    out
}

/// Read `path` from `offset`, extract observations, and return the new offset.
///
/// Stops at the last newline in what it read: a transcript is appended to
/// live, so the tail of a read can be half a line. The unread remainder is
/// picked up next poll, when it is complete.
fn read_from(
    path: &Path,
    offset: u64,
    scanner: &mut Scanner,
    out: &mut Vec<Observation>,
) -> Option<u64> {
    let mut file = std::fs::File::open(path).ok()?;
    if offset > 0 {
        file.seek(SeekFrom::Start(offset)).ok()?;
    }
    let mut bytes = Vec::new();
    if let Err(error) = file.read_to_end(&mut bytes) {
        if error.kind() != ErrorKind::Interrupted {
            return None;
        }
    }
    let end = bytes.iter().rposition(|b| *b == b'\n')? + 1;
    let mut text = String::from_utf8_lossy(&bytes[..end]).into_owned();
    // A mid-run tail-back lands in the middle of a line; that fragment is not
    // valid JSON and would be discarded anyway, but dropping it here keeps the
    // scanner's input honest.
    if offset > 0 && text.starts_with(|c: char| c != '{') {
        if let Some(newline) = text.find('\n') {
            text = text.split_off(newline + 1);
        }
    }
    scanner.feed(&text, out);
    Some(offset + end as u64)
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

    #[test]
    fn reading_stops_at_the_last_complete_line() {
        let dir = std::env::temp_dir().join(format!("eavesdrop-read-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        let title = r#"{"type":"ai-title","aiTitle":"T","sessionId":"s1"}"#;
        std::fs::write(&path, format!("{title}\n{{\"partial\"")).unwrap();

        let mut scanner = Scanner::new();
        let mut out = Vec::new();
        let offset = read_from(&path, 0, &mut scanner, &mut out).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(offset, title.len() as u64 + 1);

        // The partial line completes; the next read picks it up whole.
        std::fs::write(&path, format!("{title}\n{title}\n")).unwrap();
        let offset = read_from(&path, offset, &mut scanner, &mut out).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(offset, (title.len() as u64 + 1) * 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_file_with_no_newline_yet_yields_nothing_and_no_offset() {
        let dir = std::env::temp_dir().join(format!("eavesdrop-nonl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        std::fs::write(&path, "{\"half").unwrap();
        let mut scanner = Scanner::new();
        let mut out = Vec::new();
        assert!(read_from(&path, 0, &mut scanner, &mut out).is_none());
        assert!(out.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_mid_line_tail_back_drops_the_leading_fragment() {
        let dir = std::env::temp_dir().join(format!("eavesdrop-tailback-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        let title = r#"{"type":"ai-title","aiTitle":"T","sessionId":"s1"}"#;
        std::fs::write(&path, format!("{title}\n{title}\n")).unwrap();

        let mut scanner = Scanner::new();
        let mut out = Vec::new();
        // Start 10 bytes into the first line, as a tail-back would.
        read_from(&path, 10, &mut scanner, &mut out).unwrap();
        assert_eq!(out.len(), 1, "only the one whole line should be seen");
        std::fs::remove_dir_all(&dir).ok();
    }
}
