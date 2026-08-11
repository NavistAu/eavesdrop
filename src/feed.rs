//! Turning one-sided observations into whole messages, and naming the agents.
//!
//! A single exchange is written to two transcripts: the sender records a
//! confirmed `SendMessage`, the recipient records an inbound envelope. Neither
//! side carries the other's session id — the sender addresses a registry
//! *name*, the recipient is told a conversation *title* — so the only thing
//! linking the two records is the message body, which is byte-identical
//! across them. Pairing on the body is what lets a row name both ends even
//! when one of them has since exited.
//!
//! Pairing is bounded to `PAIR_WINDOW`: the two records land tens of
//! milliseconds apart in practice (63 ms in the observed case), and the window
//! stops a genuinely repeated message from being folded into its earlier twin.

use crate::extract::Observation;
use crate::registry::Registry;
use chrono::{DateTime, Utc};
use std::collections::HashMap;

/// How far apart a send and its matching receive may be and still be one
/// message. Generous against clock skew, tight enough that a resent identical
/// body is a second row.
const PAIR_WINDOW_SECS: i64 = 60;

/// Which sides of an exchange were actually observed. A row that saw only one
/// side is still worth showing — the other transcript is simply outside the
/// window, or belongs to a session that never wrote one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Corroboration {
    /// Both transcripts seen: the message was sent and it arrived.
    Both,
    /// Only the sender's confirmed record.
    SentOnly,
    /// Only the recipient's inbound record.
    RecvOnly,
}

/// One cross-session message, as fully as it could be reconstructed.
#[derive(Debug, Clone)]
pub struct Message {
    pub ts: DateTime<Utc>,
    pub from_session: Option<String>,
    pub from_title: Option<String>,
    pub to_session: Option<String>,
    /// The target as addressed, e.g. `tmux-bridge-b6 [ab12cd]`.
    pub to_target: Option<String>,
    pub summary: Option<String>,
    pub body: String,
    /// The sender's confirmed `SendMessage` record was seen.
    pub sent_seen: bool,
    /// The recipient's inbound envelope was seen.
    pub recv_seen: bool,
}

impl Message {
    pub fn corroboration(&self) -> Corroboration {
        match (self.sent_seen, self.recv_seen) {
            (true, true) => Corroboration::Both,
            (true, false) => Corroboration::SentOnly,
            _ => Corroboration::RecvOnly,
        }
    }

    /// Stable identity for "already rendered", so a message re-read from a
    /// re-discovered file is not printed twice.
    pub fn key(&self) -> String {
        format!("{}\u{1}{}", self.ts.timestamp_millis(), self.body)
    }

    /// First non-empty line of the body, for the one-line form.
    pub fn first_line(&self) -> &str {
        self.body
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
    }
}

/// Accumulates observations, pairs the two sides of each exchange, and
/// remembers enough about each session to label it.
pub struct Feed {
    registry: Registry,
    titles: HashMap<String, String>,
    by_title: HashMap<String, String>,
    cwds: HashMap<String, String>,
    /// Messages not yet handed to the renderer.
    buffer: Vec<Message>,
}

impl Feed {
    pub fn new(registry: Registry) -> Self {
        Self {
            registry,
            titles: HashMap::new(),
            by_title: HashMap::new(),
            cwds: HashMap::new(),
            buffer: Vec::new(),
        }
    }

    pub fn ingest(&mut self, observations: Vec<Observation>) {
        for observation in observations {
            match observation {
                Observation::Title { session_id, title } => {
                    self.by_title.insert(title.clone(), session_id.clone());
                    self.titles.insert(session_id, title);
                }
                Observation::Sent(sent) => {
                    let Some(ts) = parse_ts(&sent.ts) else {
                        continue;
                    };
                    if let Some(cwd) = sent.from_cwd {
                        self.cwds.insert(sent.from_session.clone(), cwd);
                    }
                    if let Some(existing) = self.find_pair(ts, &sent.body, |m| !m.sent_seen) {
                        existing.from_session = Some(sent.from_session);
                        existing.to_target = Some(sent.to_target);
                        existing.summary = sent.summary;
                        existing.sent_seen = true;
                        // The send is written first; prefer its timestamp so
                        // the row sits where the exchange actually began.
                        existing.ts = ts;
                        continue;
                    }
                    self.buffer.push(Message {
                        ts,
                        from_session: Some(sent.from_session),
                        from_title: None,
                        to_session: None,
                        to_target: Some(sent.to_target),
                        summary: sent.summary,
                        body: sent.body,
                        sent_seen: true,
                        recv_seen: false,
                    });
                }
                Observation::Recv(recv) => {
                    let Some(ts) = parse_ts(&recv.ts) else {
                        continue;
                    };
                    if let Some(existing) = self.find_pair(ts, &recv.body, |m| !m.recv_seen) {
                        existing.to_session = Some(recv.to_session);
                        existing.from_title = Some(recv.from_title);
                        existing.recv_seen = true;
                        continue;
                    }
                    self.buffer.push(Message {
                        ts,
                        from_session: None,
                        from_title: Some(recv.from_title),
                        to_session: Some(recv.to_session),
                        to_target: None,
                        summary: None,
                        body: recv.body,
                        sent_seen: false,
                        recv_seen: true,
                    });
                }
            }
        }
    }

    /// The buffered message with this exact body, within the pairing window,
    /// whose missing side `wants` reports. Searched newest-first so a repeated
    /// body pairs with its nearest twin.
    fn find_pair(
        &mut self,
        ts: DateTime<Utc>,
        body: &str,
        wants: impl Fn(&Message) -> bool,
    ) -> Option<&mut Message> {
        let index = self.buffer.iter().rposition(|m| {
            m.body == body && wants(m) && (m.ts - ts).num_seconds().abs() <= PAIR_WINDOW_SECS
        })?;
        self.buffer.get_mut(index)
    }

    /// Remove and return every buffered message older than `grace`, oldest
    /// first. Holding recent ones back lets the second half of an exchange
    /// arrive before the row is printed and becomes unrewritable.
    pub fn take_ready(&mut self, now: DateTime<Utc>, grace_ms: i64) -> Vec<Message> {
        let (ready, held): (Vec<_>, Vec<_>) = std::mem::take(&mut self.buffer)
            .into_iter()
            .partition(|m| (now - m.ts).num_milliseconds() >= grace_ms);
        self.buffer = held;
        let mut ready = ready;
        ready.sort_by_key(|m| m.ts);
        ready
    }

    /// Endpoint keys of a message, used to group agents into conversations.
    /// A session id when one is known, the addressed name or title otherwise —
    /// so a message whose far end never wrote a transcript still groups.
    pub fn endpoints(&self, message: &Message) -> (String, String) {
        (self.sender_key(message), self.recipient_key(message))
    }

    fn sender_key(&self, message: &Message) -> String {
        if let Some(session) = &message.from_session {
            return session.clone();
        }
        if let Some(title) = &message.from_title {
            if let Some(session) = self.by_title.get(title) {
                return session.clone();
            }
            return format!("title:{title}");
        }
        "?".into()
    }

    fn recipient_key(&self, message: &Message) -> String {
        if let Some(session) = &message.to_session {
            return session.clone();
        }
        if let Some(target) = &message.to_target {
            if let Some(session) = self.registry.session_for_target(target) {
                return session.to_string();
            }
            return format!("name:{}", bare_target(target));
        }
        "?".into()
    }

    pub fn sender_label(&self, message: &Message) -> String {
        if let Some(session) = &message.from_session {
            return self.label_of(session);
        }
        if let Some(title) = &message.from_title {
            return match self.by_title.get(title) {
                Some(session) => self.label_of(session),
                None => truncate(title, 28),
            };
        }
        "?".into()
    }

    pub fn recipient_label(&self, message: &Message) -> String {
        if let Some(session) = &message.to_session {
            return self.label_of(session);
        }
        match &message.to_target {
            Some(target) => match self.registry.session_for_target(target) {
                Some(session) => self.label_of(session),
                None => bare_target(target).to_string(),
            },
            None => "?".into(),
        }
    }

    /// Best available name for a session. The registry has the name the agents
    /// themselves use, but only while the session is alive; everything after
    /// it is reconstruction, ending at a short id that is at least unique.
    fn label_of(&self, session: &str) -> String {
        if let Some(name) = self.registry.name_of(session) {
            return name.to_string();
        }
        let cwd = self
            .cwds
            .get(session)
            .map(String::as_str)
            .or_else(|| self.registry.cwd_of(session));
        if let Some(base) = cwd.and_then(|c| c.rsplit('/').find(|s| !s.is_empty())) {
            return format!("{base}-{}", short(session, 4));
        }
        if let Some(title) = self.titles.get(session) {
            return truncate(title, 28);
        }
        short(session, 8).to_string()
    }
}

fn bare_target(target: &str) -> &str {
    target.split_once(" [").map_or(target, |(n, _)| n).trim()
}

fn short(session: &str, n: usize) -> &str {
    &session[..n.min(session.len())]
}

fn truncate(text: &str, n: usize) -> String {
    if text.chars().count() <= n {
        return text.to_string();
    }
    let head: String = text.chars().take(n.saturating_sub(1)).collect();
    format!("{head}\u{2026}")
}

pub fn parse_ts(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::{Recv, Sent};

    fn feed() -> Feed {
        Feed::new(Registry::empty())
    }

    fn sent(ts: &str, body: &str) -> Observation {
        Observation::Sent(Sent {
            ts: ts.into(),
            from_session: "bbbb2222-aaaa".into(),
            from_cwd: Some("/home/user/project".into()),
            to_target: "tmux-bridge-b6 [ab12cd]".into(),
            summary: Some("Demo message".into()),
            body: body.into(),
        })
    }

    fn recv(ts: &str, body: &str) -> Observation {
        Observation::Recv(Recv {
            ts: ts.into(),
            to_session: "aaaa1111-bbbb".into(),
            from_title: "Optimize tmux-bridge token efficiency".into(),
            body: body.into(),
        })
    }

    fn drain(f: &mut Feed) -> Vec<Message> {
        f.take_ready(parse_ts("2030-01-01T00:00:00Z").unwrap(), 0)
    }

    #[test]
    fn the_two_sides_of_one_exchange_collapse_to_one_message() {
        let mut f = feed();
        f.ingest(vec![
            sent("2026-08-10T20:34:16.184Z", "hello there"),
            recv("2026-08-10T20:34:16.247Z", "hello there"),
        ]);
        let got = drain(&mut f);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].corroboration(), Corroboration::Both);
        assert_eq!(got[0].from_session.as_deref(), Some("bbbb2222-aaaa"));
        assert_eq!(got[0].to_session.as_deref(), Some("aaaa1111-bbbb"));
        // The send's timestamp wins — the exchange began there.
        assert_eq!(got[0].ts, parse_ts("2026-08-10T20:34:16.184Z").unwrap());
    }

    #[test]
    fn pairing_works_in_either_arrival_order() {
        let mut f = feed();
        f.ingest(vec![
            recv("2026-08-10T20:34:16.247Z", "hello there"),
            sent("2026-08-10T20:34:16.184Z", "hello there"),
        ]);
        assert_eq!(drain(&mut f).len(), 1);
    }

    #[test]
    fn an_identical_body_resent_much_later_is_a_second_message() {
        let mut f = feed();
        f.ingest(vec![
            sent("2026-08-10T20:34:16.184Z", "ping"),
            recv("2026-08-10T20:34:16.247Z", "ping"),
            sent("2026-08-10T21:00:00.000Z", "ping"),
            recv("2026-08-10T21:00:00.060Z", "ping"),
        ]);
        assert_eq!(drain(&mut f).len(), 2);
    }

    #[test]
    fn a_lone_send_still_reports_as_a_message() {
        let mut f = feed();
        f.ingest(vec![sent("2026-08-10T20:34:16.184Z", "hello")]);
        let got = drain(&mut f);
        assert_eq!(got[0].corroboration(), Corroboration::SentOnly);
        // The `[ab12cd]` ref is a disambiguation aid, not part of the name.
        assert_eq!(f.recipient_label(&got[0]), "tmux-bridge-b6");
    }

    #[test]
    fn a_lone_receive_still_reports_as_a_message() {
        let mut f = feed();
        f.ingest(vec![recv("2026-08-10T20:34:16.247Z", "hello")]);
        let got = drain(&mut f);
        assert_eq!(got[0].corroboration(), Corroboration::RecvOnly);
    }

    #[test]
    fn a_title_record_names_a_sender_that_left_no_send_record() {
        let mut f = feed();
        f.ingest(vec![
            Observation::Title {
                session_id: "bbbb2222-aaaa".into(),
                title: "Optimize tmux-bridge token efficiency".into(),
            },
            recv("2026-08-10T20:34:16.247Z", "hello"),
        ]);
        let got = drain(&mut f);
        // Resolved through the title index to a session, then to a cwd-less
        // label — and crucially the endpoint key is the session id, so it
        // groups with that session's other traffic.
        assert_eq!(f.endpoints(&got[0]).0, "bbbb2222-aaaa");
    }

    #[test]
    fn an_unresolvable_sender_still_gets_a_stable_group_key() {
        let mut f = feed();
        f.ingest(vec![recv("2026-08-10T20:34:16.247Z", "hello")]);
        let got = drain(&mut f);
        assert_eq!(
            f.endpoints(&got[0]).0,
            "title:Optimize tmux-bridge token efficiency"
        );
    }

    #[test]
    fn label_falls_back_to_cwd_basename_then_short_id() {
        let mut f = feed();
        f.ingest(vec![sent("2026-08-10T20:34:16.184Z", "hello")]);
        let got = drain(&mut f);
        assert_eq!(got[0].clone().from_session.unwrap(), "bbbb2222-aaaa");
        assert_eq!(f.sender_label(&got[0]), "project-bbbb");

        let mut g = feed();
        g.ingest(vec![Observation::Sent(Sent {
            ts: "2026-08-10T20:34:16.184Z".into(),
            from_session: "abcdef01-2345".into(),
            from_cwd: None,
            to_target: "peer".into(),
            summary: None,
            body: "x".into(),
        })]);
        let got = drain(&mut g);
        assert_eq!(g.sender_label(&got[0]), "abcdef01");
    }

    #[test]
    fn grace_holds_recent_messages_and_releases_old_ones() {
        let mut f = feed();
        let now = parse_ts("2026-08-10T20:34:17.000Z").unwrap();
        f.ingest(vec![sent("2026-08-10T20:34:16.900Z", "fresh")]);
        f.ingest(vec![sent("2026-08-10T20:30:00.000Z", "stale")]);
        let ready = f.take_ready(now, 750);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].body, "stale");
        assert_eq!(f.take_ready(now, 0).len(), 1);
    }

    #[test]
    fn ready_messages_come_out_in_timestamp_order() {
        let mut f = feed();
        f.ingest(vec![
            sent("2026-08-10T21:00:00.000Z", "second"),
            sent("2026-08-10T20:00:00.000Z", "first"),
        ]);
        let got = drain(&mut f);
        assert_eq!(got[0].body, "first");
        assert_eq!(got[1].body, "second");
    }

    #[test]
    fn first_line_skips_leading_blank_lines() {
        let mut f = feed();
        f.ingest(vec![sent(
            "2026-08-10T20:00:00.000Z",
            "\n\n  real line\nmore",
        )]);
        assert_eq!(drain(&mut f)[0].first_line(), "  real line");
    }

    #[test]
    fn truncate_is_char_safe() {
        assert_eq!(truncate("abc", 5), "abc");
        assert_eq!(
            truncate("\u{2026}\u{2026}\u{2026}\u{2026}", 3),
            "\u{2026}\u{2026}\u{2026}"
        );
    }

    #[test]
    fn short_id_never_panics_on_a_short_session_id() {
        assert_eq!(short("ab", 8), "ab");
    }
}
