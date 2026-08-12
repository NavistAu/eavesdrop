//! Finding transcripts and reading the bytes they grow by.
//!
//! Split out from the output paths because there are two of them — a plain
//! stream for pipes and one-shot history, and a TUI that redraws — and both
//! need exactly the same scanning behaviour underneath.

use crate::extract::{Observation, Scanner};
use std::io::{ErrorKind, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// How often the whole projects tree is rescanned for files that have started
/// moving. Coarser than the caller's poll because it stats every transcript on
/// the machine (~2,000 of them) rather than the handful being followed.
pub const REDISCOVER: std::time::Duration = std::time::Duration::from_secs(2);

/// How much of a newly-active transcript to read back. A file that starts
/// growing mid-run was not read at startup, so its triggering message is
/// already on disk; this recovers it without re-reading a multi-megabyte
/// history.
const TAIL_BACK: u64 = 256 * 1024;

pub struct Watcher {
    projects: PathBuf,
    /// Byte offset already consumed, per transcript being followed.
    tracked: std::collections::HashMap<PathBuf, u64>,
    scanner: Scanner,
    last_rediscover: SystemTime,
}

impl Watcher {
    pub fn new(projects: PathBuf) -> Self {
        Self {
            projects,
            tracked: std::collections::HashMap::new(),
            scanner: Scanner::new(),
            last_rediscover: SystemTime::now(),
        }
    }

    /// Read every transcript touched inside the window, in full, and report how
    /// many there were.
    ///
    /// The mtime filter is what makes this affordable — a day's window is ~26
    /// files out of ~2,000, and ~120 MB out of ~2.5 GB.
    pub fn scan_initial(&mut self, cutoff: SystemTime, out: &mut Vec<Observation>) -> usize {
        let mut scanned = 0;
        for (path, mtime, _) in discover(&self.projects) {
            if mtime < cutoff {
                continue;
            }
            if let Some(offset) = read_from(&path, 0, &mut self.scanner, out) {
                self.tracked.insert(path, offset);
                scanned += 1;
            }
        }
        scanned
    }

    /// Read whatever the followed transcripts have grown by, rediscovering the
    /// tree on its own slower cadence.
    pub fn poll(&mut self, out: &mut Vec<Observation>) {
        if self.last_rediscover.elapsed().unwrap_or_default() >= REDISCOVER {
            self.last_rediscover = SystemTime::now();
            for (path, _, len) in discover(&self.projects) {
                self.tracked
                    .entry(path)
                    .or_insert_with(|| len.saturating_sub(TAIL_BACK));
            }
        }

        for (path, offset) in self.tracked.iter_mut() {
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
            if let Some(new_offset) = read_from(path, *offset, &mut self.scanner, out) {
                *offset = new_offset;
            }
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

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
