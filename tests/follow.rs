//! End-to-end cover for the parts unit tests cannot reach: the follow loop,
//! the CLI surface, and the fact that one exchange spans two files that are
//! appended to independently while the process is running.
//!
//! Every fixture here is a synthetic `$CLAUDE_CONFIG_DIR`, so these tests do
//! not read the machine's real transcripts and do not depend on which sessions
//! happen to be alive.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::Duration;

const BIN: &str = env!("CARGO_BIN_EXE_eavesdrop");
/// Generous: the follow loop polls at 400 ms, rediscovers at 2 s, and holds
/// each message for a 2.5 s grace before printing.
const WAIT: Duration = Duration::from_secs(12);

struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "eavesdrop-it-{tag}-{}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("projects").join("p")).unwrap();
        std::fs::create_dir_all(dir.join("sessions")).unwrap();
        Self { dir }
    }

    fn transcript(&self, name: &str) -> PathBuf {
        self.dir
            .join("projects")
            .join("p")
            .join(format!("{name}.jsonl"))
    }

    fn append(&self, name: &str, line: &str) {
        let path = self.transcript(name);
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        writeln!(file, "{line}").unwrap();
    }

    fn session(&self, pid: u32, session_id: &str, name: &str, pane: &str) {
        std::fs::write(
            self.dir.join("sessions").join(format!("{pid}.json")),
            format!(
                r#"{{"pid":{pid},"sessionId":"{session_id}","name":"{name}","cwd":"/w/{name}","tmux":"main:@1.{pane}"}}"#
            ),
        )
        .unwrap();
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        Command::new(BIN)
            .args(args)
            .env("CLAUDE_CONFIG_DIR", &self.dir)
            .env("NO_COLOR", "1")
            .output()
            .unwrap()
    }

    fn spawn(&self, args: &[&str]) -> Follower {
        let mut child = Command::new(BIN)
            .args(args)
            .env("CLAUDE_CONFIG_DIR", &self.dir)
            .env("NO_COLOR", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Follower { child, lines: rx }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

struct Follower {
    child: Child,
    lines: Receiver<String>,
}

impl Follower {
    fn next_line(&self) -> Result<String, RecvTimeoutError> {
        self.lines.recv_timeout(WAIT)
    }

    /// A header line plus `body_lines` following body lines, joined. Leading
    /// blanks are skipped: every message is preceded by a separator line. The
    /// body count is passed in because a body ends only when the next message
    /// starts or the feed goes quiet, which a reader cannot tell apart in time.
    fn next_message(&self, body_lines: usize) -> Result<String, RecvTimeoutError> {
        let mut out = loop {
            let line = self.next_line()?;
            if !line.trim().is_empty() {
                break line;
            }
        };
        for _ in 0..body_lines {
            out.push('\n');
            out.push_str(&self.next_line()?);
        }
        Ok(out)
    }
}

impl Drop for Follower {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

/// Messages printed, counted by header lines. A message renders as one header
/// at column 0 followed by its body indented, so counting raw lines counts
/// paragraphs rather than messages.
fn message_count(stdout: &str) -> usize {
    stdout
        .lines()
        .filter(|l| !l.starts_with(' ') && !l.is_empty())
        .count()
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn send_use(ts: &str, tool_id: &str, from: &str, to: &str, summary: &str, body: &str) -> String {
    format!(
        r#"{{"type":"assistant","timestamp":"{ts}","sessionId":"{from}","cwd":"/w/{from}","message":{{"content":[{{"type":"tool_use","id":"{tool_id}","name":"SendMessage","input":{{"to":"{to}","summary":"{summary}","message":"{body}"}}}}]}}}}"#
    )
}

fn send_ok(tool_id: &str) -> String {
    format!(
        r#"{{"type":"user","message":{{"content":[{{"tool_use_id":"{tool_id}","type":"tool_result","content":[]}}]}},"toolUseResult":{{"success":true,"message":"ok","msg_id":"m-{tool_id}"}}}}"#
    )
}

fn enqueue(ts: &str, to_session: &str, from_title: &str, body: &str) -> String {
    format!(
        r#"{{"type":"queue-operation","operation":"enqueue","timestamp":"{ts}","sessionId":"{to_session}","content":"<cross-session-message from-name=\"{from_title}\" from-mode=\"prompting\">\n{body}\n</cross-session-message>"}}"#
    )
}

fn title(session_id: &str, title: &str) -> String {
    format!(r#"{{"type":"ai-title","aiTitle":"{title}","sessionId":"{session_id}"}}"#)
}

#[test]
fn a_live_exchange_written_to_two_files_prints_as_one_row() {
    let fixture = Fixture::new("live");
    fixture.append("a", &title("aaaa1111", "Supervisor doing the rounds"));
    fixture.append("b", &title("bbbb2222", "Worker two"));

    let follower = fixture.spawn(&["--since", "1h"]);
    // Let the startup scan finish and the follow loop take over.
    std::thread::sleep(Duration::from_millis(1200));

    let ts = now();
    fixture.append(
        "a",
        &send_use(
            &ts,
            "toolu_1",
            "aaaa1111",
            "worker-two",
            "unrendered-summary",
            "Rerun the suite",
        ),
    );
    fixture.append("a", &send_ok("toolu_1"));
    std::thread::sleep(Duration::from_millis(300));
    fixture.append(
        "b",
        &enqueue(
            &ts,
            "bbbb2222",
            "Supervisor doing the rounds",
            "Rerun the suite",
        ),
    );

    let line = follower
        .next_message(1)
        .expect("a message should be printed");
    assert!(line.contains("Rerun the suite"), "{line}");
    // The sender's summary must never reach the column: only the sending side
    // records one, so using it would make the text mean different things
    // depending on which transcript happened to be scanned.
    assert!(!line.contains("unrendered-summary"), "{line}");
    // Both ends named, and no caveat: the two halves were folded into one row.
    assert!(
        line.contains("Worker two"),
        "recipient resolved via title: {line}"
    );
    assert!(
        !line.contains("not in window"),
        "should be corroborated: {line}"
    );

    // And exactly one row — the two records must not print twice.
    assert!(
        follower.next_line().is_err(),
        "a single exchange must print a single row"
    );
}

#[test]
fn a_history_only_run_reads_what_is_already_on_disk() {
    let fixture = Fixture::new("history");
    let ts = now();
    fixture.append(
        "a",
        &send_use(
            &ts,
            "toolu_1",
            "aaaa1111",
            "peer",
            "unrendered-summary",
            "Landed it",
        ),
    );
    fixture.append("a", &send_ok("toolu_1"));

    let out = fixture.run(&["--since", "1h", "--no-follow"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success());
    assert_eq!(message_count(&stdout), 1, "{stdout}");
    assert!(stdout.contains("Landed it"), "{stdout}");
    assert!(!stdout.contains("unrendered-summary"), "{stdout}");
    assert!(
        stdout.contains("(sent; recipient transcript not in window)"),
        "{stdout}"
    );
}

#[test]
fn the_since_window_excludes_older_messages() {
    let fixture = Fixture::new("window");
    let old = (chrono::Utc::now() - chrono::Duration::hours(3))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    fixture.append(
        "a",
        &send_use(&old, "toolu_1", "aaaa1111", "peer", "s-old", "Ancient"),
    );
    fixture.append("a", &send_ok("toolu_1"));
    let recent = now();
    fixture.append(
        "a",
        &send_use(&recent, "toolu_2", "aaaa1111", "peer", "s-new", "Recent"),
    );
    fixture.append("a", &send_ok("toolu_2"));

    let stdout = String::from_utf8(fixture.run(&["--since", "1h", "--no-follow"]).stdout).unwrap();
    assert!(stdout.contains("Recent"), "{stdout}");
    assert!(!stdout.contains("Ancient"), "{stdout}");
}

#[test]
fn tail_keeps_only_the_most_recent_rows() {
    let fixture = Fixture::new("tail");
    for i in 0..4 {
        let ts = (chrono::Utc::now() - chrono::Duration::minutes(10 - i))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let id = format!("toolu_{i}");
        fixture.append(
            "a",
            &send_use(
                &ts,
                &id,
                "aaaa1111",
                "peer",
                &format!("s{i}"),
                &format!("msg{i}"),
            ),
        );
        fixture.append("a", &send_ok(&id));
    }
    let stdout = String::from_utf8(
        fixture
            .run(&["--since", "1h", "--no-follow", "--tail", "2"])
            .stdout,
    )
    .unwrap();
    assert_eq!(message_count(&stdout), 2, "{stdout}");
    assert!(
        stdout.contains("msg2") && stdout.contains("msg3"),
        "{stdout}"
    );
    assert!(!stdout.contains("msg0"), "{stdout}");
}

#[test]
fn json_output_is_one_object_per_message() {
    let fixture = Fixture::new("json");
    let ts = now();
    fixture.append(
        "a",
        &send_use(&ts, "toolu_1", "aaaa1111", "peer", "Landed it", "the body"),
    );
    fixture.append("a", &send_ok("toolu_1"));
    fixture.append("b", &enqueue(&ts, "bbbb2222", "Some Title", "the body"));

    let out = fixture.run(&["--since", "1h", "--no-follow", "--json"]);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        stdout.lines().count(),
        1,
        "one JSON object per line: {stdout}"
    );
    let value: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(value["from_session"], "aaaa1111");
    assert_eq!(value["to_session"], "bbbb2222");
    assert_eq!(value["body"], "the body");
    assert_eq!(value["corroboration"], "both");
    // A JSON run must not have a human banner mixed into stdout.
    assert!(!stdout.contains("eavesdrop:"), "{stdout}");
}

#[test]
fn the_whole_body_prints_with_no_flag_to_ask_for_it() {
    let fixture = Fixture::new("body");
    let ts = now();
    fixture.append(
        "a",
        &send_use(
            &ts,
            "toolu_1",
            "aaaa1111",
            "peer",
            "Sum",
            "line one\\nline two\\nline three",
        ),
    );
    fixture.append("a", &send_ok("toolu_1"));
    let stdout = String::from_utf8(fixture.run(&["--since", "1h", "--no-follow"]).stdout).unwrap();
    assert!(
        stdout.contains("\n    line one\n    line two\n    line three"),
        "{stdout:?}"
    );
    // The header names both ends; the body carries the words. Nothing elided.
    assert!(!stdout.contains('\u{2026}'), "{stdout:?}");
    assert!(
        !stdout.contains("Sum"),
        "summary must not appear: {stdout:?}"
    );
}

#[test]
fn registry_names_beat_reconstructed_ones() {
    let fixture = Fixture::new("names");
    fixture.session(1, "aaaa1111", "boss-agent", "%10");
    let ts = now();
    fixture.append(
        "a",
        &send_use(&ts, "toolu_1", "aaaa1111", "peer", "Sum", "b"),
    );
    fixture.append("a", &send_ok("toolu_1"));
    let stdout = String::from_utf8(fixture.run(&["--since", "1h", "--no-follow"]).stdout).unwrap();
    assert!(stdout.contains("boss-agent"), "{stdout}");
}

#[test]
fn a_failed_send_never_reaches_the_feed() {
    let fixture = Fixture::new("failed");
    let ts = now();
    fixture.append(
        "a",
        &send_use(&ts, "toolu_1", "aaaa1111", "peer", "Never sent", "b"),
    );
    fixture.append(
        "a",
        r#"{"type":"user","message":{"content":[{"tool_use_id":"toolu_1","type":"tool_result","content":[]}]},"toolUseResult":{"success":false,"message":"'peer' is not an agent in this conversation."}}"#,
    );
    let stdout = String::from_utf8(fixture.run(&["--since", "1h", "--no-follow"]).stdout).unwrap();
    assert!(stdout.trim().is_empty(), "{stdout}");
}

#[test]
fn a_bad_since_is_refused_rather_than_guessed() {
    let fixture = Fixture::new("badsince");
    let out = fixture.run(&["--since", "fortnight", "--no-follow"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot read --since"));
}

#[test]
fn a_missing_projects_directory_is_an_error_not_a_panic() {
    let dir = std::env::temp_dir().join(format!("eavesdrop-empty-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let out = Command::new(BIN)
        .args(["--no-follow"])
        .env("CLAUDE_CONFIG_DIR", &dir)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no transcripts at"));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_transcript_that_only_starts_moving_mid_run_is_picked_up() {
    let fixture = Fixture::new("newfile");
    fixture.append("a", &title("aaaa1111", "Supervisor"));
    let follower = fixture.spawn(&["--since", "1h"]);
    std::thread::sleep(Duration::from_millis(1200));

    // A transcript that did not exist at startup, so it was never scanned and
    // is only reachable through the rediscovery pass.
    let ts = now();
    fixture.append(
        "late",
        &send_use(&ts, "toolu_9", "cccc3333", "peer", "s", "From nowhere"),
    );
    fixture.append("late", &send_ok("toolu_9"));

    let line = follower
        .next_message(1)
        .expect("rediscovery should find the new file");
    assert!(line.contains("From nowhere"), "{line}");
}

#[test]
fn the_projects_tree_is_read_but_never_written() {
    let fixture = Fixture::new("readonly");
    let ts = now();
    fixture.append(
        "a",
        &send_use(&ts, "toolu_1", "aaaa1111", "peer", "Sum", "b"),
    );
    fixture.append("a", &send_ok("toolu_1"));
    let before = snapshot(&fixture.dir);
    fixture.run(&["--since", "1h", "--no-follow"]);
    assert_eq!(
        before,
        snapshot(&fixture.dir),
        "eavesdrop must not modify anything"
    );
}

/// Every file under `root` with its length, for an unchanged-tree assertion.
fn snapshot(root: &Path) -> Vec<(PathBuf, u64)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push((path, entry.metadata().unwrap().len()));
            }
        }
    }
    out.sort();
    out
}
