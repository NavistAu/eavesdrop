//! Pulling message records out of session transcripts.
//!
//! A transcript is JSONL at `<config_dir>/projects/<cwd-slug>/<session-id>.jsonl`
//! and contains everything a session ever did. This module extracts *only*
//! cross-session message records and the session-title records used to label
//! them. It must not drift into being a general transcript reader — that is a
//! different tool with different privacy properties.
//!
//! Every line is substring-filtered before it is handed to `serde_json`. A
//! day's window is ~120 MB across ~26 files on a working machine; parsing all
//! of it as JSON costs seconds, scanning it for four literal markers costs
//! milliseconds.
//!
//! ## The three record shapes (verified 2026-08-11)
//!
//! **Send, step 1** — an assistant `tool_use`:
//! ```text
//! {"type":"assistant","timestamp":…,"sessionId":…,"cwd":…,"message":{"content":[
//!   {"type":"tool_use","id":"toolu_…","name":"SendMessage",
//!    "input":{"to":"tmux-bridge-b6 [ab12cd]","summary":…,"message":…}}]}}
//! ```
//! `input` also carries `content`/`recipient` duplicating `message`/`to`. Only
//! `message`/`to` are read here; the duplicates are not relied on.
//!
//! **Send, step 2** — the matching `tool_result`, which is what makes the send
//! real. A `tool_use` alone proves nothing: the first observed demo send failed
//! on an ambiguous name and had to be re-sent with a `[ref]`. Rendering
//! unconfirmed `tool_use` records would show messages that were never
//! delivered. `toolUseResult.msg_id` is present **only** on a successful
//! cross-session send — the same tool addressed to an in-session subagent
//! returns `pin`/`resumedAgentId` instead, which is how the two mechanisms are
//! told apart here.
//!
//! **Receive** — a `queue-operation` enqueue whose `content` is the
//! `<cross-session-message>` envelope. Claude Code also writes a `user` record
//! carrying the same envelope when the message reaches the model, at a
//! *slightly different* timestamp (`.247Z` vs `.321Z` in the observed pair), so
//! the two cannot be deduped on time. Only the enqueue is read, which sidesteps
//! the duplicate entirely — and avoids matching prose, since any session
//! *discussing* cross-session messaging has the marker in its ordinary text.

use serde::Deserialize;
use std::collections::HashMap;

/// One thing learned from one transcript line.
#[derive(Debug, Clone)]
pub enum Observation {
    /// A session's generated conversation title. This is what appears as
    /// `from-name` on the receiving side, so it is the only bridge from an
    /// inbound envelope back to the session that sent it.
    Title {
        session_id: String,
        title: String,
    },
    Sent(Sent),
    Recv(Recv),
}

/// A confirmed outbound message, seen from the sender's transcript.
#[derive(Debug, Clone)]
pub struct Sent {
    pub ts: String,
    pub from_session: String,
    pub from_cwd: Option<String>,
    /// The target as addressed, e.g. `tmux-bridge-b6 [ab12cd]`.
    pub to_target: String,
    pub summary: Option<String>,
    pub body: String,
}

/// How an inbound envelope identifies whoever sent it. Two forms occur, and
/// neither is a session id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sender {
    /// `from-name="…"` — the sender's *conversation title*, not its registry
    /// name. Written when a person's session addressed a peer by name.
    Title(String),
    /// `from="uds:…"` — the sender's inbox socket path, written when one agent
    /// messages another. The reply is addressed straight back to this path, so
    /// an agent-to-agent thread never mentions a name at all.
    Socket(String),
}

/// An inbound message, seen from the recipient's transcript.
#[derive(Debug, Clone)]
pub struct Recv {
    pub ts: String,
    pub to_session: String,
    pub from: Sender,
    pub body: String,
}

const MARK_ENVELOPE: &str = "<cross-session-message";
const MARK_SEND: &str = "\"SendMessage\"";
const MARK_MSG_ID: &str = "\"msg_id\"";
const MARK_TITLE: &str = "\"ai-title\"";

#[derive(Deserialize)]
struct TitleRec {
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    #[serde(rename = "aiTitle")]
    ai_title: Option<String>,
}

#[derive(Deserialize)]
struct QueueRec {
    operation: Option<String>,
    timestamp: Option<String>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    content: Option<String>,
}

#[derive(Deserialize)]
struct AssistantRec {
    timestamp: Option<String>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    cwd: Option<String>,
    message: Option<AssistantBody>,
}

#[derive(Deserialize)]
struct AssistantBody {
    content: Option<Vec<Block>>,
}

#[derive(Deserialize)]
struct Block {
    #[serde(rename = "type")]
    kind: Option<String>,
    id: Option<String>,
    name: Option<String>,
    input: Option<SendInput>,
    #[serde(rename = "tool_use_id")]
    tool_use_id: Option<String>,
}

#[derive(Deserialize)]
struct SendInput {
    to: Option<String>,
    summary: Option<String>,
    message: Option<String>,
}

/// The `user` record carrying a `tool_result`, used only to confirm a send.
#[derive(Deserialize)]
struct ResultRec {
    message: Option<AssistantBody>,
    #[serde(rename = "toolUseResult")]
    tool_use_result: Option<ToolUseResult>,
}

#[derive(Deserialize)]
struct ToolUseResult {
    success: Option<bool>,
    /// Present only on a successful *cross-session* send.
    msg_id: Option<String>,
}

/// Holds `tool_use` records until their `tool_result` confirms delivery.
///
/// Keyed by `toolu_…` id, which is globally unique, so one scanner can span
/// every transcript being watched. Unconfirmed entries are simply never
/// emitted; they cost a few hundred bytes each and are dropped when the
/// scanner is.
#[derive(Default)]
pub struct Scanner {
    pending: HashMap<String, Sent>,
}

impl Scanner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Extract every observation in `chunk`, which must contain whole lines.
    pub fn feed(&mut self, chunk: &str, out: &mut Vec<Observation>) {
        for line in chunk.lines() {
            self.feed_line(line, out);
        }
    }

    /// Each marker is tested independently, with no early return.
    ///
    /// A record's own *content* can quote another record's marker. An agent
    /// that writes "carrying a `<cross-session-message>` envelope" into a
    /// message body produces a `SendMessage` line containing the envelope
    /// marker; dispatching on the first marker that matches routed that line
    /// to the envelope parser, which rejected it, and the send was never seen
    /// at all. This tool dropped exactly one of its own messages that way.
    ///
    /// Trying every extractor is safe because each one validates the record
    /// type it needs, so a mismatch is a cheap no-op rather than a wrong
    /// parse: an enqueue has no `message.content` blocks, and an assistant
    /// record is not an enqueue.
    fn feed_line(&mut self, line: &str, out: &mut Vec<Observation>) {
        if line.contains(MARK_TITLE) {
            if let Ok(rec) = serde_json::from_str::<TitleRec>(line) {
                if let (Some(session_id), Some(title)) = (rec.session_id, rec.ai_title) {
                    out.push(Observation::Title { session_id, title });
                }
            }
        }
        if line.contains(MARK_ENVELOPE) {
            self.feed_envelope(line, out);
        }
        if line.contains(MARK_SEND) {
            self.feed_send(line);
        }
        if line.contains(MARK_MSG_ID) {
            self.feed_send_result(line, out);
        }
    }

    fn feed_envelope(&mut self, line: &str, out: &mut Vec<Observation>) {
        let Ok(rec) = serde_json::from_str::<QueueRec>(line) else {
            return;
        };
        if rec.operation.as_deref() != Some("enqueue") {
            return;
        }
        let (Some(ts), Some(to_session), Some(content)) =
            (rec.timestamp, rec.session_id, rec.content)
        else {
            return;
        };
        let Some(envelope) = parse_envelope(&content) else {
            return;
        };
        out.push(Observation::Recv(Recv {
            ts,
            to_session,
            from: envelope.from,
            body: envelope.body,
        }));
    }

    fn feed_send(&mut self, line: &str) {
        let Ok(rec) = serde_json::from_str::<AssistantRec>(line) else {
            return;
        };
        let (Some(ts), Some(from_session)) = (rec.timestamp, rec.session_id) else {
            return;
        };
        let Some(blocks) = rec.message.and_then(|m| m.content) else {
            return;
        };
        for block in blocks {
            if block.kind.as_deref() != Some("tool_use")
                || block.name.as_deref() != Some("SendMessage")
            {
                continue;
            }
            let (Some(id), Some(input)) = (block.id, block.input) else {
                continue;
            };
            let (Some(to_target), Some(body)) = (input.to, input.message) else {
                continue;
            };
            self.pending.insert(
                id,
                Sent {
                    ts: ts.clone(),
                    from_session: from_session.clone(),
                    from_cwd: rec.cwd.clone(),
                    to_target,
                    summary: input.summary,
                    body,
                },
            );
        }
    }

    fn feed_send_result(&mut self, line: &str, out: &mut Vec<Observation>) {
        let Ok(rec) = serde_json::from_str::<ResultRec>(line) else {
            return;
        };
        let Some(result) = rec.tool_use_result else {
            return;
        };
        // `msg_id` marks a cross-session delivery; a subagent send returns
        // `pin`/`resumedAgentId` and must not appear in this feed.
        if result.msg_id.is_none() || result.success != Some(true) {
            return;
        }
        let Some(blocks) = rec.message.and_then(|m| m.content) else {
            return;
        };
        for block in blocks {
            if block.kind.as_deref() != Some("tool_result") {
                continue;
            }
            let Some(id) = block.tool_use_id else {
                continue;
            };
            if let Some(sent) = self.pending.remove(&id) {
                out.push(Observation::Sent(sent));
            }
        }
    }
}

struct Envelope {
    from: Sender,
    body: String,
}

/// Split `<cross-session-message …>BODY</cross-session-message>`.
///
/// Two attribute sets occur and both must parse. A send addressed by name
/// arrives as `from-name="<title>" from-mode="…"`; an agent replying to a peer
/// arrives as `from="uds:<socket>" hop-chain="…" from-mode="…"`. Requiring
/// `from-name` silently dropped every agent-to-agent receive, which is exactly
/// the traffic worth watching.
///
/// Deliberately a literal scan rather than an XML parse: the envelope is
/// generated, not user-authored, and the body is free prose that may itself
/// contain angle brackets.
fn parse_envelope(content: &str) -> Option<Envelope> {
    let start = content.find(MARK_ENVELOPE)?;
    let rest = &content[start..];
    let open_end = rest.find('>')?;
    let attrs = &rest[..open_end];
    // `from-name` is checked first: `attr` matches on a substring, and the
    // key "from" would otherwise also match inside `from-name="…"`.
    let from = match attr(attrs, "from-name") {
        Some(title) => Sender::Title(title),
        None => {
            let raw = attr(attrs, "from")?;
            Sender::Socket(raw.strip_prefix("uds:").unwrap_or(&raw).to_string())
        }
    };
    let after_open = &rest[open_end + 1..];
    let body = match after_open.find("</cross-session-message>") {
        Some(close) => &after_open[..close],
        None => after_open,
    };
    Some(Envelope {
        from,
        body: body.trim().to_string(),
    })
}

/// Read `name="value"` out of an opening tag. Values are titles, which never
/// contain a quote in practice; a quote would truncate the name rather than
/// derail the parse.
fn attr(attrs: &str, name: &str) -> Option<String> {
    let key = format!("{name}=\"");
    let at = attrs.find(&key)? + key.len();
    let rest = &attrs[at..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(lines: &[&str]) -> Vec<Observation> {
        let mut scanner = Scanner::new();
        let mut out = Vec::new();
        scanner.feed(&lines.join("\n"), &mut out);
        out
    }

    const ENQUEUE: &str = r#"{"type":"queue-operation","operation":"enqueue","timestamp":"2026-08-10T20:34:16.247Z","sessionId":"aaaa1111","content":"<cross-session-message from-name=\"Optimize tmux-bridge token efficiency for TUIs\" from-mode=\"prompting\">\nhello there\n</cross-session-message>"}"#;

    const SEND_USE: &str = r#"{"type":"assistant","timestamp":"2026-08-10T20:34:16.184Z","sessionId":"bbbb2222","cwd":"/home/user/project","message":{"content":[{"type":"tool_use","id":"toolu_A","name":"SendMessage","input":{"to":"tmux-bridge-b6 [ab12cd]","summary":"Demo message","message":"hello there","content":"hello there","recipient":"tmux-bridge-b6 [ab12cd]"}}]}}"#;

    const SEND_OK: &str = r#"{"type":"user","message":{"content":[{"tool_use_id":"toolu_A","type":"tool_result","content":[]}]},"toolUseResult":{"success":true,"message":"sent","msg_id":"aaaa9999"}}"#;

    #[test]
    fn enqueue_yields_a_receive_with_sender_title_and_trimmed_body() {
        let got = collect(&[ENQUEUE]);
        assert_eq!(got.len(), 1);
        let Observation::Recv(r) = &got[0] else {
            panic!("expected Recv, got {got:?}")
        };
        assert_eq!(r.to_session, "aaaa1111");
        assert_eq!(
            r.from,
            Sender::Title("Optimize tmux-bridge token efficiency for TUIs".into())
        );
        assert_eq!(r.body, "hello there");
        assert_eq!(r.ts, "2026-08-10T20:34:16.247Z");
    }

    #[test]
    fn send_is_emitted_only_once_its_result_confirms_it() {
        assert!(
            collect(&[SEND_USE]).is_empty(),
            "unconfirmed send must not emit"
        );
        let got = collect(&[SEND_USE, SEND_OK]);
        assert_eq!(got.len(), 1);
        let Observation::Sent(s) = &got[0] else {
            panic!("expected Sent, got {got:?}")
        };
        assert_eq!(s.from_session, "bbbb2222");
        assert_eq!(s.to_target, "tmux-bridge-b6 [ab12cd]");
        assert_eq!(s.summary.as_deref(), Some("Demo message"));
        assert_eq!(s.body, "hello there");
        // The send's own timestamp, not the result's.
        assert_eq!(s.ts, "2026-08-10T20:34:16.184Z");
    }

    #[test]
    fn failed_send_is_dropped() {
        // The real ambiguous-name failure: success false, and no msg_id.
        let failed = r#"{"type":"user","message":{"content":[{"tool_use_id":"toolu_A","type":"tool_result","content":[]}]},"toolUseResult":{"success":false,"message":"'tmux-bridge-b6' is not an agent in this conversation."}}"#;
        assert!(collect(&[SEND_USE, failed]).is_empty());
    }

    #[test]
    fn subagent_send_is_not_cross_session_traffic() {
        // Same tool, addressed to an in-session agent: `pin`, never `msg_id`.
        let use_line = r#"{"type":"assistant","timestamp":"2026-08-05T06:00:10.820Z","sessionId":"eeee5555","message":{"content":[{"type":"tool_use","id":"toolu_B","name":"SendMessage","input":{"to":"dddd4444dddd4444d","summary":"poll","message":"drive the loop"}}]}}"#;
        let result = r#"{"type":"user","message":{"content":[{"tool_use_id":"toolu_B","type":"tool_result","content":[]}]},"toolUseResult":{"success":true,"message":"queued","pin":{"id":"dddd4444dddd4444d","ref":"a3a5f8"}}}"#;
        assert!(collect(&[use_line, result]).is_empty());
    }

    #[test]
    fn the_duplicate_user_delivery_record_is_ignored() {
        // Same envelope, ~74ms later, as a `user` record. Counting it would
        // double every inbound message.
        let user_copy = r#"{"type":"user","timestamp":"2026-08-10T20:34:16.321Z","sessionId":"aaaa1111","isMeta":true,"message":{"role":"user","content":"Another Claude session sent a message:\n<cross-session-message from-name=\"Optimize tmux-bridge token efficiency for TUIs\" from-mode=\"prompting\">\nhello there\n</cross-session-message>\n\nThis came from another Claude session."}}"#;
        assert_eq!(collect(&[ENQUEUE, user_copy]).len(), 1);
    }

    #[test]
    fn a_send_whose_body_quotes_the_envelope_marker_is_still_a_send() {
        // Observed live: this tool messaged a peer *about* the envelope
        // format, and the marker inside its own body made the line parse as
        // an inbound envelope instead of an outbound send. The message
        // vanished from the feed.
        let use_line = r#"{"type":"assistant","timestamp":"2026-08-12T14:01:04.488Z","sessionId":"ffff6666","cwd":"/w/e","message":{"content":[{"type":"tool_use","id":"toolu_C","name":"SendMessage","input":{"to":"tmux-bridge-b6 [ab12cd]","summary":"Demo","message":"your transcript records it as an enqueue carrying a <cross-session-message> envelope"}}]}}"#;
        let result = r#"{"type":"user","message":{"content":[{"tool_use_id":"toolu_C","type":"tool_result","content":[]}]},"toolUseResult":{"success":true,"message":"ok","msg_id":"92a8f607"}}"#;
        let got = collect(&[use_line, result]);
        assert_eq!(got.len(), 1, "the send must survive quoting the marker");
        let Observation::Sent(s) = &got[0] else {
            panic!("expected Sent, got {got:?}")
        };
        assert_eq!(s.to_target, "tmux-bridge-b6 [ab12cd]");
    }

    #[test]
    fn prose_mentioning_the_marker_is_not_a_message() {
        let prose = r#"{"type":"assistant","timestamp":"2026-08-11T04:23:15.508Z","sessionId":"ffff6666","message":{"content":[{"type":"text","text":"Match on the literal marker <cross-session-message to find them."}]}}"#;
        assert!(collect(&[prose]).is_empty());
    }

    #[test]
    fn title_records_are_collected() {
        let line = r#"{"type":"ai-title","aiTitle":"Optimize tmux-bridge token efficiency for TUIs","sessionId":"bbbb2222"}"#;
        let got = collect(&[line]);
        let Observation::Title { session_id, title } = &got[0] else {
            panic!("expected Title")
        };
        assert_eq!(session_id, "bbbb2222");
        assert_eq!(title, "Optimize tmux-bridge token efficiency for TUIs");
    }

    #[test]
    fn malformed_lines_never_derail_a_scan() {
        let got = collect(&["", "{ not json", r#"{"type":"ai-title"}"#, ENQUEUE]);
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn an_agent_to_agent_envelope_names_its_sender_by_socket() {
        // Observed live between two peers. No `from-name` at all: the sender is
        // an inbox path and there is an extra `hop-chain` attribute. Requiring
        // `from-name` dropped every one of these, which is the traffic worth
        // watching in the first place.
        let line = r#"{"type":"queue-operation","operation":"enqueue","timestamp":"2026-08-12T14:14:55.421Z","sessionId":"11112222","content":"<cross-session-message from=\"uds:/var/folders/b9/T/cc-socks/41696.sock\" hop-chain=\"3333444455556666,7777888899990000\" from-mode=\"prompting\">\nClaude Code 2.1.228\n</cross-session-message>"}"#;
        let got = collect(&[line]);
        assert_eq!(got.len(), 1);
        let Observation::Recv(r) = &got[0] else {
            panic!("expected Recv, got {got:?}")
        };
        assert_eq!(
            r.from,
            Sender::Socket("/var/folders/b9/T/cc-socks/41696.sock".into()),
            "the uds: prefix is an address scheme, not part of the path"
        );
        assert_eq!(r.body, "Claude Code 2.1.228");
    }

    #[test]
    fn from_name_wins_over_a_substring_match_on_from() {
        // `attr` matches a substring, so the key "from" also occurs inside
        // `from-name="…"`. Checking `from-name` first is what keeps a named
        // sender from being read as a socket address.
        let e = parse_envelope(
            "<cross-session-message from-name=\"Some Title\" from-mode=\"prompting\">\nbody\n</cross-session-message>",
        )
        .unwrap();
        assert_eq!(e.from, Sender::Title("Some Title".into()));
    }

    #[test]
    fn envelope_body_keeps_its_own_angle_brackets() {
        let e = parse_envelope(
            "<cross-session-message from-name=\"T\" from-mode=\"prompting\">\nuse <div> tags\n</cross-session-message>",
        )
        .unwrap();
        assert_eq!(e.body, "use <div> tags");
        assert_eq!(e.from, Sender::Title("T".into()));
    }

    #[test]
    fn envelope_without_a_closing_tag_still_yields_its_body() {
        let e = parse_envelope("<cross-session-message from-name=\"T\">\ntruncated").unwrap();
        assert_eq!(e.body, "truncated");
    }
}
