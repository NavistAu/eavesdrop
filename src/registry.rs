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
}

/// A live session, as far as the registry knows it.
#[derive(Debug, Clone)]
pub struct Entry {
    pub session_id: String,
    pub name: String,
    pub cwd: Option<String>,
    /// The `%N` component of the entry's `tmux` field, when it has one.
    pub pane: Option<String>,
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

/// Registry entries keyed by session id, and by name for reverse lookup.
pub struct Registry {
    by_session: HashMap<String, Entry>,
    by_name: HashMap<String, String>,
}

impl Registry {
    pub fn load() -> Self {
        Self::from_entries(load())
    }

    #[cfg(test)]
    /// A registry that knows nothing — every label falls through to
    /// reconstruction. Used by tests that must not depend on which sessions
    /// happen to be alive on the machine running them.
    pub fn empty() -> Self {
        Self::from_entries(Vec::new())
    }

    pub fn from_entries(entries: Vec<Entry>) -> Self {
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
        }
    }

    pub fn name_of(&self, session_id: &str) -> Option<&str> {
        self.by_session.get(session_id).map(|e| e.name.as_str())
    }

    pub fn cwd_of(&self, session_id: &str) -> Option<&str> {
        self.by_session
            .get(session_id)
            .and_then(|e| e.cwd.as_deref())
    }

    /// Resolve a `SendMessage` target back to a session id. Targets are
    /// written either bare (`tmux-bridge-b6`) or disambiguated with a ref
    /// (`tmux-bridge-b6 [ab12cd]`); the ref is a display aid, so strip it.
    pub fn session_for_target(&self, target: &str) -> Option<&str> {
        let bare = target.split_once(" [").map_or(target, |(n, _)| n).trim();
        self.by_name.get(bare).map(String::as_str)
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

/// Pane ids of the tmux window this process is running in, or `None` when not
/// under tmux. `list-panes` with no target defaults to the current window.
pub fn current_window_panes() -> Option<Vec<String>> {
    std::env::var("TMUX").ok()?;
    let out = Command::new("tmux")
        .args(["list-panes", "-F", "#{pane_id}"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect(),
    )
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
        Registry::from_entries(entries)
    }

    fn entry(name: &str, sid: &str, pane: Option<&str>) -> Entry {
        Entry {
            session_id: sid.into(),
            name: name.into(),
            cwd: None,
            pane: pane.map(String::from),
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
