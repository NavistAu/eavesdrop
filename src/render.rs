//! Grouping agents into conversations, and drawing a line per message.
//!
//! The interleaving is the point of the feed, so two agents talking are only
//! legible if their rows share a colour that nothing else uses. A conversation
//! here is a connected component of the who-talked-to-whom graph: every
//! message unions its two endpoints, and the component gets one colour. A
//! supervisor with four workers is therefore one colour, not five — which is
//! what you want when the question is "who is coordinating with whom".
//!
//! Colour is assigned when a component is first seen and never reassigned. If
//! two separate components later merge — the first message between two
//! previously unconnected groups — the older component's colour survives, so
//! rows already on screen never come to mean something different from what
//! they meant when they were printed.

use crate::feed::{Corroboration, Feed, Message};
use chrono::Local;
use std::collections::HashMap;

/// 256-colour codes, chosen to stay distinguishable on both light and dark
/// terminals. Reused modulo its length once conversations outnumber it.
const PALETTE: [u8; 8] = [39, 214, 78, 170, 179, 45, 205, 149];

/// Labels are padded to this width so the arrows line up into a column.
const LABEL_WIDTH: usize = 18;

/// Union-find over endpoint keys, with a colour per component.
#[derive(Default)]
pub struct Groups {
    parent: HashMap<String, String>,
    /// Insertion order, which decides which root survives a merge.
    seq: HashMap<String, usize>,
    /// Colour slot, held by the root of each component.
    colour: HashMap<String, usize>,
    next: usize,
}

impl Groups {
    pub fn new() -> Self {
        Self::default()
    }

    fn touch(&mut self, key: &str) {
        if self.seq.contains_key(key) {
            return;
        }
        self.parent.insert(key.to_string(), key.to_string());
        self.seq.insert(key.to_string(), self.next);
        self.colour.insert(key.to_string(), self.next);
        self.next += 1;
    }

    fn find(&mut self, key: &str) -> String {
        let mut root = key.to_string();
        while let Some(parent) = self.parent.get(&root) {
            if parent == &root {
                break;
            }
            root = parent.clone();
        }
        // Path compression: walk again, repointing everything at the root.
        let mut cursor = key.to_string();
        while cursor != root {
            let next = self
                .parent
                .insert(cursor, root.clone())
                .unwrap_or_else(|| root.clone());
            cursor = next;
        }
        root
    }

    /// Union the two endpoints of a message and return their shared colour.
    pub fn colour_of(&mut self, a: &str, b: &str) -> u8 {
        self.touch(a);
        self.touch(b);
        let (ra, rb) = (self.find(a), self.find(b));
        let root = if ra == rb {
            ra
        } else if self.seq[&ra] <= self.seq[&rb] {
            self.parent.insert(rb, ra.clone());
            ra
        } else {
            self.parent.insert(ra, rb.clone());
            rb
        };
        PALETTE[self.colour[&root] % PALETTE.len()]
    }
}

fn paint(colour_on: bool, code: u8, bold: bool, text: &str) -> String {
    if !colour_on {
        return text.to_string();
    }
    let weight = if bold { ";1" } else { "" };
    format!("\u{1b}[38;5;{code}{weight}m{text}\u{1b}[0m")
}

fn dim(colour_on: bool, text: &str) -> String {
    if !colour_on {
        return text.to_string();
    }
    format!("\u{1b}[2m{text}\u{1b}[0m")
}

/// Pad to the arrow column, truncating labels that would break it.
fn cell(label: &str, right_align: bool) -> String {
    let label = if label.chars().count() > LABEL_WIDTH {
        let head: String = label.chars().take(LABEL_WIDTH - 1).collect();
        format!("{head}\u{2026}")
    } else {
        label.to_string()
    };
    let pad = LABEL_WIDTH - label.chars().count();
    if right_align {
        format!("{}{}", " ".repeat(pad), label)
    } else {
        format!("{}{}", label, " ".repeat(pad))
    }
}

/// Why a row could not be corroborated from both transcripts. `None` when it
/// was — the common case, which stays unannotated.
fn caveat(corroboration: Corroboration) -> Option<&'static str> {
    match corroboration {
        Corroboration::Both => None,
        Corroboration::SentOnly => Some("(sent; recipient transcript not in window)"),
        Corroboration::RecvOnly => Some("(received; sender transcript not in window)"),
    }
}

/// A message as a header line naming both ends, then the message itself.
///
/// The body always prints in full. This is a log of what agents said to each
/// other, so the words are the content and everything else is addressing; a
/// one-line extract turns the feed into an index of a conversation you then
/// cannot read. The sender's `summary` is never shown — only the sending side
/// records one, so it would make the text mean different things depending on
/// which transcript fell inside the scan window. `--json` still carries it.
pub fn render(feed: &Feed, groups: &mut Groups, message: &Message, colour_on: bool) -> String {
    let (from_key, to_key) = feed.endpoints(message);
    let colour = groups.colour_of(&from_key, &to_key);

    let time = message
        .ts
        .with_timezone(&Local)
        .format("%H:%M:%S")
        .to_string();
    let from = paint(
        colour_on,
        colour,
        true,
        &cell(&feed.sender_label(message), true),
    );
    // The recipient is padded to the column only when a caveat follows it.
    // Otherwise it ends the header, and padding would leave trailing spaces
    // inside the colour escape where no trim can reach them.
    let caveat = caveat(message.corroboration());
    let recipient = feed.recipient_label(message);
    let to = paint(
        colour_on,
        colour,
        false,
        &match caveat {
            Some(_) => cell(&recipient, false),
            None => recipient,
        },
    );
    let arrow = paint(colour_on, colour, false, "\u{2192}");

    let mut out = format!("{}  {from} {arrow} {to}", dim(colour_on, &time));
    if let Some(caveat) = caveat {
        out.push_str(&format!("  {}", dim(colour_on, caveat)));
    }
    // Body at full weight, not dimmed: it is the thing being read, and the
    // indent alone is enough to hold it apart from the header above it.
    for body_line in message.body.lines() {
        out.push('\n');
        out.push_str(&format!("        {body_line}"));
    }
    out
}

/// One JSON object per message, for piping into something else. Hand-built
/// rather than derived: the wire shape is a deliberate contract, not a mirror
/// of the internal struct.
pub fn json(feed: &Feed, message: &Message) -> String {
    let value = serde_json::json!({
        "ts": message.ts.to_rfc3339(),
        "from": feed.sender_label(message),
        "from_session": message.from_session,
        "to": feed.recipient_label(message),
        "to_session": message.to_session,
        "summary": message.summary,
        "body": message.body,
        "corroboration": match message.corroboration() {
            Corroboration::Both => "both",
            Corroboration::SentOnly => "sent-only",
            Corroboration::RecvOnly => "recv-only",
        },
    });
    value.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::{Observation, Sent};
    use crate::feed::parse_ts;
    use crate::registry::Registry;

    fn msg(from: &str, to: &str) -> Message {
        Message {
            ts: parse_ts("2026-08-10T20:34:16.184Z").unwrap(),
            from_session: Some(from.into()),
            from_hint: None,
            to_session: Some(to.into()),
            to_target: Some(to.into()),
            summary: Some("hi".into()),
            body: "hi there".into(),
            sent_seen: true,
            recv_seen: true,
        }
    }

    #[test]
    fn one_pair_shares_a_colour_in_both_directions() {
        let mut g = Groups::new();
        let there = g.colour_of("a", "b");
        let back = g.colour_of("b", "a");
        assert_eq!(there, back);
    }

    #[test]
    fn unrelated_pairs_get_different_colours() {
        let mut g = Groups::new();
        assert_ne!(g.colour_of("a", "b"), g.colour_of("c", "d"));
    }

    #[test]
    fn a_supervisor_and_its_workers_are_one_conversation() {
        let mut g = Groups::new();
        let first = g.colour_of("boss", "w1");
        assert_eq!(g.colour_of("boss", "w2"), first);
        assert_eq!(g.colour_of("boss", "w3"), first);
        // And a worker replying to the boss stays in the same group.
        assert_eq!(g.colour_of("w2", "boss"), first);
    }

    #[test]
    fn merging_two_groups_keeps_the_older_groups_colour() {
        let mut g = Groups::new();
        let old = g.colour_of("a", "b");
        let new = g.colour_of("c", "d");
        assert_ne!(old, new);
        // First contact between the two groups: rows already printed in the
        // older colour must not have been retroactively wrong.
        assert_eq!(g.colour_of("b", "c"), old);
        assert_eq!(g.colour_of("d", "a"), old);
    }

    #[test]
    fn palette_wraps_without_panicking() {
        let mut g = Groups::new();
        for i in 0..(PALETTE.len() * 3) {
            g.colour_of(&format!("x{i}"), &format!("y{i}"));
        }
    }

    #[test]
    fn a_corroborated_line_carries_no_caveat() {
        let feed = Feed::new(Registry::empty());
        let mut g = Groups::new();
        let out = render(&feed, &mut g, &msg("aaaa1111", "bbbb2222"), false);
        assert!(out.contains("\u{2192}"), "{out}");
        assert!(out.ends_with("hi there"), "{out}");
    }

    #[test]
    fn a_one_sided_line_says_which_side_is_missing() {
        let feed = Feed::new(Registry::empty());
        let mut g = Groups::new();
        let mut m = msg("aaaa1111", "bbbb2222");
        m.recv_seen = false;
        let out = render(&feed, &mut g, &m, false);
        assert!(out.contains("recipient transcript not in window"), "{out}");
    }

    #[test]
    fn the_body_is_shown_never_the_senders_summary() {
        // Only the sending side records a summary, so showing it would make
        // the text depend on which transcript happened to be scanned.
        let feed = Feed::new(Registry::empty());
        let mut g = Groups::new();
        let mut m = msg("aaaa1111", "bbbb2222");
        m.summary = Some("A tidy summary".into());
        m.body = "what was actually said".into();
        let out = render(&feed, &mut g, &m, false);
        assert!(out.ends_with("what was actually said"), "{out}");
        assert!(!out.contains("A tidy summary"), "{out}");
    }

    #[test]
    fn a_long_body_prints_whole_never_an_extract() {
        let feed = Feed::new(Registry::empty());
        let mut g = Groups::new();
        let mut m = msg("aaaa1111", "bbbb2222");
        m.body = "first line\n\nmiddle line\nlast line".into();
        let out = render(&feed, &mut g, &m, false);
        for expected in ["first line", "middle line", "last line"] {
            assert!(out.contains(expected), "missing {expected:?}: {out}");
        }
        assert!(out.ends_with("        last line"), "{out}");
        // Nothing is elided, so nothing may claim to be.
        assert!(!out.contains('\u{2026}'), "{out}");
    }

    #[test]
    fn a_blank_line_inside_a_body_is_preserved_as_structure() {
        let feed = Feed::new(Registry::empty());
        let mut g = Groups::new();
        let mut m = msg("aaaa1111", "bbbb2222");
        m.body = "para one\n\npara two".into();
        let out = render(&feed, &mut g, &m, false);
        assert!(
            out.contains("        para one\n        \n        para two"),
            "{out:?}"
        );
    }

    #[test]
    fn every_body_line_is_indented_under_the_header() {
        let feed = Feed::new(Registry::empty());
        let mut g = Groups::new();
        let mut m = msg("aaaa1111", "bbbb2222");
        m.body = "one\ntwo".into();
        let out = render(&feed, &mut g, &m, false);
        assert!(out.contains("\n        one\n        two"), "{out:?}");
    }

    #[test]
    fn colour_off_emits_no_escapes() {
        let feed = Feed::new(Registry::empty());
        let mut g = Groups::new();
        let out = render(&feed, &mut g, &msg("aaaa1111", "bbbb2222"), false);
        assert!(!out.contains('\u{1b}'), "{out:?}");
    }

    #[test]
    fn colour_on_emits_escapes() {
        let feed = Feed::new(Registry::empty());
        let mut g = Groups::new();
        let out = render(&feed, &mut g, &msg("aaaa1111", "bbbb2222"), true);
        assert!(out.contains("\u{1b}[38;5;"), "{out:?}");
    }

    #[test]
    fn cell_truncates_an_over_long_label_to_the_column_width() {
        let long = "a".repeat(40);
        assert_eq!(cell(&long, false).chars().count(), LABEL_WIDTH);
        assert_eq!(cell("short", true).chars().count(), LABEL_WIDTH);
    }

    #[test]
    fn json_names_both_ends_and_the_corroboration() {
        let mut feed = Feed::new(Registry::empty());
        feed.ingest(vec![Observation::Sent(Sent {
            ts: "2026-08-10T20:34:16.184Z".into(),
            from_session: "bbbb2222-aaaa".into(),
            from_cwd: Some("/home/user/project".into()),
            to_target: "tmux-bridge-b6 [ab12cd]".into(),
            summary: Some("Demo".into()),
            body: "hello".into(),
        })]);
        let ready = feed.take_ready(parse_ts("2030-01-01T00:00:00Z").unwrap(), 0);
        let out = json(&feed, &ready[0]);
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["from"], "project-bbbb");
        assert_eq!(parsed["to"], "tmux-bridge-b6");
        assert_eq!(parsed["corroboration"], "sent-only");
        assert_eq!(parsed["body"], "hello");
    }
}
