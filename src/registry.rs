//! The live-session registry, and the tmux panes of the current window.
//!
//! `<config_dir>/sessions/<pid>.json` is what `ListAgents` reads. It is an
//! undocumented implementation detail whose schema has already drifted once
//! (it gained `tmux`, `messagingSocketPath` and `waitingFor` mid-project), so
//! every field here is optional and every read degrades to "nothing learned"
//! rather than erroring.
//!
//! Only live sessions have an entry. Historical messages routinely name
//! sessions that have since exited, so the registry is a source of *better*
//! labels, never the source of truth about who exists.

use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// `$CLAUDE_CONFIG_DIR` when set and non-empty, `~/.claude` otherwise.
pub fn config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".claude")
}

#[derive(Debug, Clone, Deserialize)]
struct RawEntry {
    name: Option<String>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    cwd: Option<String>,
    /// `<session>:@<window>.<%pane>`, e.g. `"main:@9.%196"`.
    tmux: Option<String>,
    /// The session's inbox socket. Absent for sessions that can send but not
    /// receive — and it is how an agent-to-agent reply addresses its peer, so
    /// it is also the only handle some inbound envelopes carry.
    #[serde(rename = "messagingSocketPath")]
    messaging_socket_path: Option<String>,
}

/// A live session, as far as the registry knows it.
#[derive(Debug, Clone)]
pub struct Entry {
    pub session_id: String,
    pub name: String,
    pub cwd: Option<String>,
    /// The `%N` component of the entry's `tmux` field, when it has one.
    pub pane: Option<String>,
    /// Path to the session's inbox socket, when it has one.
    pub socket: Option<String>,
}

/// Every readable, usable entry under `<config_dir>/sessions/`. A missing
/// directory, an unreadable file, or malformed JSON contributes nothing and
/// never blocks a sibling.
pub fn load() -> Vec<Entry> {
    load_from(&config_dir().join("sessions"))
}

fn load_from(dir: &Path) -> Vec<Entry> {
    let mut out = Vec::new();
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return out;
    };
    for dir_entry in read_dir.flatten() {
        let path = dir_entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(raw) = serde_json::from_str::<RawEntry>(&text) else {
            continue;
        };
        let (Some(session_id), Some(name)) = (raw.session_id, raw.name) else {
            continue;
        };
        out.push(Entry {
            session_id,
            name,
            cwd: raw.cwd,
            pane: raw
                .tmux
                .as_deref()
                .and_then(pane_component)
                .map(String::from),
            socket: raw.messaging_socket_path,
        });
    }
    out
}

/// Extract `%N` from a `tmux` field of the form `<session>:@<window>.<%pane>`.
/// Returns `None` rather than guessing when the last segment isn't a pane id.
fn pane_component(field: &str) -> Option<&str> {
    let last = field.rsplit('.').next()?;
    let digits = last.strip_prefix('%')?;
    (!digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())).then_some(last)
}

/// What tmux knows about a pane beyond its id.
///
/// Both name fields are tmux *user options* set on the pane, and both are the
/// operator's own words rather than anything Claude Code generated — which is
/// exactly why they outrank the registry name when labelling a row.
#[derive(Debug, Clone, Default)]
pub struct PaneFacts {
    /// `@window_id`, so the panes of one window can be collected without a
    /// second tmux call.
    pub window: String,
    /// `@tb_name` — the codeword `tb mark` registered this pane under.
    pub tb_name: Option<String>,
    /// `@pane_tag` — the operator's own note-to-self, bound to `M-t` and drawn
    /// as `❖ tag` in the pane border.
    pub tag: Option<String>,
}

/// Registry entries keyed by session id, plus what tmux says about the panes
/// they occupy.
pub struct Registry {
    by_session: HashMap<String, Entry>,
    by_name: HashMap<String, String>,
    panes: HashMap<String, PaneFacts>,
}

impl Registry {
    pub fn load() -> Self {
        Self::from_parts(load(), pane_facts())
    }

    #[cfg(test)]
    /// A registry that knows nothing — every label falls through to
    /// reconstruction. Used by tests that must not depend on which sessions
    /// happen to be alive on the machine running them.
    pub fn empty() -> Self {
        Self::from_parts(Vec::new(), HashMap::new())
    }

    pub fn from_parts(entries: Vec<Entry>, panes: HashMap<String, PaneFacts>) -> Self {
        let by_name = entries
            .iter()
            .map(|e| (e.name.clone(), e.session_id.clone()))
            .collect();
        let by_session = entries
            .into_iter()
            .map(|e| (e.session_id.clone(), e))
            .collect();
        Self {
            by_session,
            by_name,
            panes,
        }
    }

    /// The operator's own name for the pane a session occupies: the `tb mark`
    /// codeword first, then the pane tag. `None` when the session has no pane,
    /// tmux is not running, or neither option is set.
    pub fn pane_label(&self, session_id: &str) -> Option<&str> {
        let pane = self.by_session.get(session_id)?.pane.as_deref()?;
        let facts = self.panes.get(pane)?;
        facts.tb_name.as_deref().or(facts.tag.as_deref())
    }

    /// Pane ids sharing a window with `$TMUX_PANE`, or `None` when not under
    /// tmux or the pane is unknown.
    ///
    /// Derived from the one `list-panes -a` call rather than asking tmux for
    /// "the current window". With no target, `list-panes` means the *client's*
    /// current window, which follows wherever the operator last looked — so
    /// `--pane` silently reported a different window after a window switch,
    /// and an empty one once the focus moved somewhere without agents.
    pub fn current_window_panes(&self) -> Option<Vec<String>> {
        let me = std::env::var("TMUX_PANE").ok()?;
        let window = &self.panes.get(&me)?.window;
        Some(
            self.panes
                .iter()
                .filter(|(_, facts)| &facts.window == window)
                .map(|(pane, _)| pane.clone())
                .collect(),
        )
    }

    pub fn name_of(&self, session_id: &str) -> Option<&str> {
        self.by_session.get(session_id).map(|e| e.name.as_str())
    }

    /// The best name the registry alone can give a session: the operator's own
    /// pane name, else the name agents address it by. `None` once the session
    /// has exited, since nothing here outlives it — callers that can do better
    /// (from a transcript's cwd or title) continue from there.
    pub fn known_label(&self, session_id: &str) -> Option<String> {
        self.pane_label(session_id)
            .or_else(|| self.name_of(session_id))
            .map(String::from)
    }

    /// `known_label`, falling back to a short session id so there is always
    /// something to print.
    pub fn label_of(&self, session_id: &str) -> String {
        self.known_label(session_id)
            .unwrap_or_else(|| session_id.chars().take(8).collect())
    }

    pub fn cwd_of(&self, session_id: &str) -> Option<&str> {
        self.by_session
            .get(session_id)
            .and_then(|e| e.cwd.as_deref())
    }

    /// Resolve a `SendMessage` target back to a session id.
    ///
    /// Three forms occur. A bare registry name (`tmux-bridge-b6`), the same
    /// disambiguated with a ref (`tmux-bridge-b6 [ab12cd]`) where the ref is a
    /// display aid to strip, and `uds:<socket path>` — which is how an agent
    /// replies to a peer, addressing the inbox it was reached from rather than
    /// any name.
    pub fn session_for_target(&self, target: &str) -> Option<&str> {
        if let Some(socket) = target.strip_prefix("uds:") {
            return self.session_for_socket(socket);
        }
        let bare = target.split_once(" [").map_or(target, |(n, _)| n).trim();
        self.by_name.get(bare).map(String::as_str)
    }

    /// Resolve an inbox socket path back to the session listening on it.
    ///
    /// Falls back to matching the file name alone, which is `<pid>.sock` and
    /// so still unique, in case the two paths disagree only on their prefix —
    /// `/var` and `/private/var` name the same directory on macOS.
    pub fn session_for_socket(&self, socket: &str) -> Option<&str> {
        let leaf = |path: &str| path.rsplit('/').next().unwrap_or(path).to_string();
        let wanted = leaf(socket);
        self.by_session
            .values()
            .find(|e| {
                e.socket
                    .as_deref()
                    .is_some_and(|s| s == socket || leaf(s) == wanted)
            })
            .map(|e| e.session_id.as_str())
    }

    /// Session ids occupying `panes`.
    pub fn sessions_in_panes(&self, panes: &[String]) -> Vec<String> {
        self.by_session
            .values()
            .filter(|e| e.pane.as_ref().is_some_and(|p| panes.contains(p)))
            .map(|e| e.session_id.clone())
            .collect()
    }
}

/// Tab-separated because a pane tag is free text the operator typed and may
/// contain spaces; a tab is the one character it will not.
const PANE_FORMAT: &str = "#{pane_id}\t#{window_id}\t#{@tb_name}\t#{@pane_tag}";

/// Every pane on the tmux server, with the two operator-set name options.
///
/// One `list-panes -a` call covers both labelling (which needs every window)
/// and the `--pane` filter (which needs one). Not being under tmux, tmux being
/// absent, and a failed call all yield an empty map — labels then fall back to
/// the registry, which is the pre-tmux behaviour.
///
/// An unset user option expands to the empty string rather than failing the
/// format, so absent marks and tags arrive as empty fields.
fn pane_facts() -> HashMap<String, PaneFacts> {
    let mut out = HashMap::new();
    if std::env::var_os("TMUX").is_none() {
        return out;
    }
    let Ok(result) = Command::new("tmux")
        .args(["list-panes", "-a", "-F", PANE_FORMAT])
        .output()
    else {
        return out;
    };
    if !result.status.success() {
        return out;
    }
    for line in String::from_utf8_lossy(&result.stdout).lines() {
        let mut fields = line.split('\t');
        let (Some(pane), Some(window)) = (fields.next(), fields.next()) else {
            continue;
        };
        if pane.is_empty() {
            continue;
        }
        out.insert(
            pane.to_string(),
            PaneFacts {
                window: window.to_string(),
                tb_name: non_empty(fields.next()),
                tag: non_empty(fields.next()),
            },
        );
    }
    out
}

fn non_empty(field: Option<&str>) -> Option<String> {
    field
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_component_extracts_percent_id() {
        assert_eq!(pane_component("main:@9.%196"), Some("%196"));
        assert_eq!(pane_component("3:@32.%188"), Some("%188"));
    }

    #[test]
    fn pane_component_rejects_non_pane_tails() {
        assert_eq!(pane_component("main:@9"), None);
        assert_eq!(pane_component("main:@9.%"), None);
        assert_eq!(pane_component("main:@9.abc"), None);
    }

    fn registry_of(entries: Vec<Entry>) -> Registry {
        Registry::from_parts(entries, HashMap::new())
    }

    fn facts(window: &str, tb_name: Option<&str>, tag: Option<&str>) -> PaneFacts {
        PaneFacts {
            window: window.into(),
            tb_name: tb_name.map(String::from),
            tag: tag.map(String::from),
        }
    }

    #[test]
    fn pane_label_prefers_the_tb_mark_then_the_tag() {
        let panes = HashMap::from([
            (
                "%10".to_string(),
                facts("@9", Some("targetagent"), Some("ignored")),
            ),
            ("%11".to_string(), facts("@9", None, Some("the-flaky-one"))),
            ("%12".to_string(), facts("@9", None, None)),
        ]);
        let reg = Registry::from_parts(
            vec![
                entry("tmux-bridge-b6", "sid-a", Some("%10")),
                entry("tmux-bridge-0c", "sid-b", Some("%11")),
                entry("tmux-bridge-d9", "sid-c", Some("%12")),
                entry("paneless", "sid-d", None),
            ],
            panes,
        );
        assert_eq!(reg.pane_label("sid-a"), Some("targetagent"));
        assert_eq!(reg.pane_label("sid-b"), Some("the-flaky-one"));
        assert_eq!(reg.pane_label("sid-c"), None, "neither option set");
        assert_eq!(reg.pane_label("sid-d"), None, "session occupies no pane");
        assert_eq!(reg.pane_label("unknown"), None);
    }

    #[test]
    fn an_empty_option_is_not_a_name() {
        // tmux expands an unset user option to "", not to a missing field.
        assert_eq!(non_empty(Some("")), None);
        assert_eq!(non_empty(Some("   ")), None);
        assert_eq!(non_empty(None), None);
        assert_eq!(non_empty(Some(" targetagent ")), Some("targetagent".into()));
    }

    fn entry(name: &str, sid: &str, pane: Option<&str>) -> Entry {
        Entry {
            session_id: sid.into(),
            name: name.into(),
            cwd: None,
            pane: pane.map(String::from),
            socket: None,
        }
    }

    #[test]
    fn target_with_ref_suffix_resolves_to_the_bare_name() {
        let reg = registry_of(vec![entry("tmux-bridge-b6", "sid-1", None)]);
        assert_eq!(
            reg.session_for_target("tmux-bridge-b6 [ab12cd]"),
            Some("sid-1")
        );
        assert_eq!(reg.session_for_target("tmux-bridge-b6"), Some("sid-1"));
        assert_eq!(reg.session_for_target("nobody"), None);
    }

    #[test]
    fn sessions_in_panes_matches_only_listed_panes() {
        let reg = registry_of(vec![
            entry("a", "sid-a", Some("%196")),
            entry("b", "sid-b", Some("%192")),
            entry("c", "sid-c", Some("%999")),
            entry("d", "sid-d", None),
        ]);
        let mut got = reg.sessions_in_panes(&["%196".into(), "%192".into()]);
        got.sort();
        assert_eq!(got, vec!["sid-a".to_string(), "sid-b".to_string()]);
    }

    #[test]
    fn missing_sessions_dir_yields_no_entries() {
        assert!(load_from(Path::new("/nonexistent/eavesdrop/sessions")).is_empty());
    }
}
