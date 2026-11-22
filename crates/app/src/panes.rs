//! The terminal views of the blocks on screen, kept in step with the server's layout.

use std::collections::HashMap;

use gpui_kit::{Entity, Task};
use rexterm::terminal::TerminalState;

#[derive(Debug, Clone, PartialEq)]
pub enum PaneStatus {
    Connecting,
    Live,
    Reconnecting,
    Exited(i64),
    Closed,
    Failed(String),
}

impl PaneStatus {
    pub fn text(&self) -> Option<String> {
        use crate::i18n::t;
        match self {
            PaneStatus::Live => None,
            PaneStatus::Connecting => Some(t("Connecting…", "正在连接…").into()),
            PaneStatus::Reconnecting => Some(t("Reconnecting…", "正在重连…").into()),
            PaneStatus::Closed => Some(t("Block closed", "这个 block 已关闭").into()),
            PaneStatus::Exited(code) => Some(format!("{} {code}", t("Program exited with code", "程序已退出,退出码"))),
            PaneStatus::Failed(why) => Some(format!("{}{why}", t("Can't connect: ", "连不上:"))),
        }
    }
}

pub struct PaneEntry {
    pub terminal: Option<Entity<TerminalState>>,
    pub status: PaneStatus,
    pub recovering: bool,
    pub tasks: Vec<Task<()>>,
}

/// block id -> its terminal view. The workspace starts and stops entries (it owns the tasks).
#[derive(Default)]
pub struct PaneSet {
    pub entries: HashMap<String, PaneEntry>,
}

impl PaneSet {
    pub fn blocks(&self) -> Vec<String> {
        self.entries.keys().cloned().collect()
    }

    pub fn terminal(&self, block_id: &str) -> Option<&Entity<TerminalState>> {
        self.entries.get(block_id).and_then(|e| e.terminal.as_ref())
    }

    pub fn status(&self, block_id: &str) -> PaneStatus {
        self.entries.get(block_id).map_or(PaneStatus::Connecting, |e| e.status.clone())
    }
}

/// What to start and what to close to go from `current` blocks to `wanted` ones.
pub fn diff(current: &[String], wanted: &[String]) -> (Vec<String>, Vec<String>) {
    let mut add: Vec<String> = Vec::new();
    for w in wanted {
        if !current.contains(w) && !add.contains(w) {
            add.push(w.clone());
        }
    }
    let remove = current.iter().filter(|c| !wanted.contains(c)).cloned().collect();
    (add, remove)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn diff_adds_and_removes() {
        let (add, remove) = diff(&v(&["a", "b"]), &v(&["b", "c"]));
        assert_eq!(add, v(&["c"]));
        assert_eq!(remove, v(&["a"]));
    }

    #[test]
    fn removed_block_is_closed() {
        let (add, remove) = diff(&v(&["a", "b", "c"]), &v(&["a", "c"]));
        assert!(add.is_empty());
        assert_eq!(remove, v(&["b"]));
    }

    #[test]
    fn reconcile_to_empty_closes_all() {
        let (add, remove) = diff(&v(&["a", "b"]), &[]);
        assert!(add.is_empty());
        assert_eq!(remove, v(&["a", "b"]));
    }

    #[test]
    fn duplicates_in_wanted_start_once() {
        let (add, _) = diff(&[], &v(&["a", "a"]));
        assert_eq!(add, v(&["a"]));
    }
}
