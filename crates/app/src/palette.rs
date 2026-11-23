//! What the command palette offers: the window's commands, the other sessions on every
//! machine, and a new session on each connected machine.

use rex_client::prefix::Command;

use crate::i18n::t;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaletteAction {
    Run(Command),
    Open { machine: String, session: String },
    NewSession(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteEntry {
    pub key: String,
    pub label: String,
    pub hint: Option<String>,
    pub action: PaletteAction,
}

pub struct PaletteSession<'a> {
    pub machine_key: &'a str,
    pub machine_name: &'a str,
    pub id: &'a str,
    pub label: &'a str,
    pub current: bool,
}

/// A session's name in lists: its label, or the end of its id.
pub fn session_name(id: &str, label: &str) -> String {
    if label.is_empty() {
        let n = id.chars().count();
        id.chars().skip(n.saturating_sub(6)).collect()
    } else {
        label.to_string()
    }
}

pub fn palette_entries(sessions: &[PaletteSession], machines: &[(&str, &str)], has_session: bool) -> Vec<PaletteEntry> {
    use Command::*;
    let commands: [(Command, &str, &str, Option<&str>, bool); 12] = [
        (NewTab, "New tab", "新建 tab", Some("c"), true),
        (SplitRight, "Split right", "向右分屏", Some("%"), true),
        (SplitDown, "Split down", "向下分屏", Some("\""), true),
        (Zoom, "Zoom pane", "放大 pane", Some("z"), true),
        (Search, "Search scrollback", "搜索回滚内容", Some("/"), true),
        (ClosePane, "Close pane", "关闭 pane", Some("x"), true),
        (RenameTab, "Rename tab", "重命名 tab", Some(","), true),
        (RenameSession, "Rename session", "重命名 session", Some("$"), true),
        (CloseSession, "Close session", "关闭 session", None, true),
        (NewSession, "New session", "新建 session", Some("N"), false),
        (ToggleSidebar, "Toggle sidebar", "显示/隐藏侧栏", Some("s"), false),
        (Settings, "Settings", "设置", None, false),
    ];
    let mut out = Vec::new();
    for (cmd, en, zh, key, needs_session) in commands {
        if needs_session && !has_session {
            continue;
        }
        out.push(PaletteEntry {
            key: format!("cmd:{cmd:?}"),
            label: t(en, zh).into(),
            hint: key.map(|k| format!("Ctrl-b {k}")),
            action: PaletteAction::Run(cmd),
        });
    }
    for s in sessions.iter().filter(|s| !s.current) {
        out.push(PaletteEntry {
            key: format!("open:{}:{}", s.machine_key, s.id),
            label: format!("{}{} / {}", t("Switch to ", "切换到 "), s.machine_name, session_name(s.id, s.label)),
            hint: None,
            action: PaletteAction::Open { machine: s.machine_key.into(), session: s.id.into() },
        });
    }
    for (key, name) in machines {
        out.push(PaletteEntry {
            key: format!("new:{key}"),
            label: format!("{}{name}", t("New session on ", "新建 session 于 ")),
            hint: None,
            action: PaletteAction::NewSession(key.to_string()),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sess<'a>(m: &'a str, name: &'a str, id: &'a str, label: &'a str, current: bool) -> PaletteSession<'a> {
        PaletteSession { machine_key: m, machine_name: name, id, label, current }
    }

    #[test]
    fn commands_need_a_session() {
        let e = palette_entries(&[], &[("local", "This computer")], false);
        let has = |a: PaletteAction| e.iter().any(|x| x.action == a);
        assert!(!has(PaletteAction::Run(Command::SplitRight)));
        assert!(!has(PaletteAction::Run(Command::ClosePane)));
        assert!(has(PaletteAction::Run(Command::Settings)));
        assert!(has(PaletteAction::NewSession("local".into())));
        let e = palette_entries(&[], &[("local", "This computer")], true);
        assert!(e.iter().any(|x| x.action == PaletteAction::Run(Command::SplitRight) && x.hint.as_deref() == Some("Ctrl-b %")));
        assert!(e.iter().any(|x| x.action == PaletteAction::Run(Command::Search) && x.hint.as_deref() == Some("Ctrl-b /")));
    }

    #[test]
    fn lists_other_sessions() {
        let s = [sess("local", "This computer", "s1", "work", true), sess("local", "This computer", "s2", "", false), sess("m:dev", "dev", "s3", "api", false)];
        let e = palette_entries(&s, &[("local", "This computer"), ("m:dev", "dev")], true);
        let opens: Vec<&PaletteEntry> = e.iter().filter(|x| matches!(x.action, PaletteAction::Open { .. })).collect();
        assert_eq!(opens.len(), 2);
        assert!(opens.iter().any(|x| x.action == PaletteAction::Open { machine: "m:dev".into(), session: "s3".into() } && x.label.contains("dev") && x.label.contains("api")));
        // A session with no label shows the end of its id.
        assert!(opens.iter().any(|x| x.label.contains("s2")));
        assert!(e.iter().any(|x| x.action == PaletteAction::NewSession("m:dev".into())));
    }

    #[test]
    fn keys_are_unique() {
        let s = [sess("local", "L", "s1", "a", true), sess("m:x", "x", "s1", "a", false)];
        let e = palette_entries(&s, &[("local", "L"), ("m:x", "x")], true);
        let mut keys: Vec<&str> = e.iter().map(|x| x.key.as_str()).collect();
        let n = keys.len();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), n);
    }
}
