//! Which machines the window shows: this computer and the ones in the settings, or only the
//! one given with -S; and where to start.

use rex_proto::transport::Endpoint;

use rex_client::model::SessionInfo;

use crate::config::{MachineConfig, Settings, WorkspaceState};
use crate::i18n::t;

#[derive(Debug, Clone)]
pub struct MachineSpec {
    /// "local", "cli" or "m:" + name: stable across runs, saved in the workspace state.
    pub key: String,
    pub name: String,
    pub endpoint: Endpoint,
    pub local: bool,
    /// The address as typed (`-S` text, the settings' endpoint, or the one built for a Rex
    /// host); empty for this computer.
    pub address: String,
    /// For a Rex host: the key of the machine whose server lists it.
    pub parent: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MachineError {
    EmptyName,
    DuplicateName,
    BadEndpoint(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnState {
    Connecting,
    Connected,
    Reconnecting,
    Failed(String),
}

/// The machines to show and the configured ones that were left out, as "name: reason".
pub fn machine_specs(cli: Option<(String, Endpoint)>, settings: &Settings, local: Endpoint) -> (Vec<MachineSpec>, Vec<String>) {
    if let Some((text, endpoint)) = cli {
        return (vec![MachineSpec { key: "cli".into(), name: text.clone(), endpoint, local: false, address: text, parent: None }], Vec::new());
    }
    let mut specs = vec![MachineSpec { key: "local".into(), name: t("This computer", "本机").into(), endpoint: local, local: true, address: String::new(), parent: None }];
    let mut warnings = Vec::new();
    let mut seen: Vec<&MachineConfig> = Vec::new();
    for m in &settings.machines {
        match validate_machine(&m.name, &m.endpoint, &seen.iter().map(|m| (*m).clone()).collect::<Vec<_>>()) {
            Ok(ok) => {
                let endpoint = Endpoint::parse(&ok.endpoint).expect("validated");
                specs.push(MachineSpec { key: format!("m:{}", ok.name), name: ok.name, endpoint, local: false, address: ok.endpoint, parent: None });
                seen.push(m);
            }
            Err(e) => warnings.push(format!("{}: {}", m.name.trim(), e.text())),
        }
    }
    (specs, warnings)
}

/// A machine as typed in the settings, trimmed, or why it can't be added.
pub fn validate_machine(name: &str, endpoint: &str, existing: &[MachineConfig]) -> Result<MachineConfig, MachineError> {
    let (name, endpoint) = (name.trim(), endpoint.trim());
    if name.is_empty() {
        return Err(MachineError::EmptyName);
    }
    if existing.iter().any(|m| m.name.trim() == name) {
        return Err(MachineError::DuplicateName);
    }
    if endpoint.is_empty() {
        return Err(MachineError::BadEndpoint(endpoint.into()));
    }
    Endpoint::parse(endpoint).map_err(|_| MachineError::BadEndpoint(endpoint.into()))?;
    Ok(MachineConfig { name: name.into(), endpoint: endpoint.into() })
}

/// The machine to show first and the session wanted there: the one named on the command line,
/// else the one shown when the app was closed (if that machine is still listed), else the first.
pub fn start_target(specs: &[MachineSpec], state: &WorkspaceState, cli_session: Option<&str>) -> (String, Option<String>) {
    let first = specs.first().map(|s| s.key.clone()).unwrap_or_default();
    if let Some(s) = cli_session {
        return (first, Some(s.to_string()));
    }
    match &state.machine {
        Some(m) if specs.iter().any(|s| &s.key == m) => (m.clone(), state.session.clone()),
        _ => (first, None),
    }
}

impl ConnState {
    /// What the sidebar says next to the machine; nothing when it is connected.
    pub fn text(&self) -> Option<String> {
        match self {
            ConnState::Connected => None,
            ConnState::Connecting => Some(t("connecting…", "正在连接…").into()),
            ConnState::Reconnecting => Some(t("reconnecting…", "正在重连…").into()),
            ConnState::Failed(why) => Some(format!("{}{why}", t("can't connect: ", "连不上:"))),
        }
    }
}

impl MachineError {
    pub fn text(&self) -> String {
        match self {
            MachineError::EmptyName => t("The name is empty.", "名称为空。").into(),
            MachineError::DuplicateName => t("A machine with this name exists.", "已有同名机器。").into(),
            MachineError::BadEndpoint(e) => format!(
                "{} \"{e}\" {}",
                t("Can't use", "无法使用地址"),
                t("(use ssh://[user@]host[:port], unix:///path or a rex server label).", "(格式:ssh://[用户@]主机[:端口]、unix:///路径 或 rex 服务器名)。")
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartChoice {
    Open(String),
    /// The session named on the command line isn't there.
    Missing(String),
    Create,
}

/// The session `wanted` names: by label or id (an id suffix of 6+ characters works).
pub fn find_session(sessions: &[SessionInfo], wanted: &str) -> Option<String> {
    sessions
        .iter()
        .find(|s| s.label == wanted || s.id == wanted || (wanted.len() >= 6 && s.id.ends_with(wanted)))
        .map(|s| s.id.clone())
}

/// What to open on the start machine: the wanted session; else (unless it was named on the
/// command line) the first; else a new one.
pub fn start_choice(sessions: &[SessionInfo], wanted: Option<&str>, strict: bool) -> StartChoice {
    if let Some(w) = wanted {
        if let Some(id) = find_session(sessions, w) {
            return StartChoice::Open(id);
        }
        if strict {
            return StartChoice::Missing(w.to_string());
        }
    }
    match sessions.first() {
        Some(s) => StartChoice::Open(s.id.clone()),
        None => StartChoice::Create,
    }
}

/// The session to show after the shown machine connected anew: the one shown before if it
/// is still there, else the first.
pub fn after_reconnect(sessions: &[SessionInfo], previous: Option<&str>) -> Option<String> {
    previous
        .and_then(|p| sessions.iter().find(|s| s.id == p))
        .or(sessions.first())
        .map(|s| s.id.clone())
}

/// The machine and session to remember: what is shown once the user has chosen it; until
/// then (an automatic fallback, or nothing shown yet) the ones saved before.
pub fn state_target(shown: Option<(&str, Option<&str>)>, saved: (Option<&str>, Option<&str>), automatic: bool) -> (Option<String>, Option<String>) {
    match shown {
        Some((m, Some(s))) if !automatic => (Some(m.to_string()), Some(s.to_string())),
        _ => (saved.0.map(str::to_string), saved.1.map(str::to_string)),
    }
}

/// The status line: the prefix hint alone; else a message, the shown machine's connection
/// state and the standing notices.
pub fn status_line(prefix: Option<&str>, banner: Option<&str>, conn: Option<&str>, notices: &[String]) -> Option<String> {
    if let Some(p) = prefix {
        return Some(p.to_string());
    }
    let parts: Vec<&str> = banner.into_iter().chain(conn).chain(notices.iter().map(String::as_str)).collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mc(name: &str, ep: &str) -> MachineConfig {
        MachineConfig { name: name.into(), endpoint: ep.into() }
    }

    fn settings(machines: Vec<MachineConfig>) -> Settings {
        Settings { machines, ..Default::default() }
    }

    fn local() -> Endpoint {
        Endpoint::Unix("/tmp/local.sock".into())
    }

    fn keys(specs: &[MachineSpec]) -> Vec<&str> {
        specs.iter().map(|s| s.key.as_str()).collect()
    }

    #[test]
    fn cli_endpoint_is_the_only_machine() {
        let s = settings(vec![mc("a", "ssh://a"), mc("b", "ssh://b")]);
        let (specs, warn) = machine_specs(Some(("unix:///tmp/t.sock".into(), Endpoint::Unix("/tmp/t.sock".into()))), &s, local());
        assert_eq!(keys(&specs), ["cli"]);
        assert_eq!(specs[0].name, "unix:///tmp/t.sock");
        assert!(warn.is_empty());
    }

    #[test]
    fn local_then_configured() {
        let s = settings(vec![mc("a", "ssh://a"), mc("b", "cmd:")]);
        let (specs, warn) = machine_specs(None, &s, local());
        assert_eq!(keys(&specs), ["local", "m:a"]);
        assert!(specs[0].local && !specs[1].local);
        assert_eq!(warn.len(), 1);
        assert!(warn[0].starts_with("b: "), "{warn:?}");
    }

    #[test]
    fn duplicate_names_skipped() {
        let s = settings(vec![mc("a", "ssh://a"), mc("a", "ssh://other")]);
        let (specs, warn) = machine_specs(None, &s, local());
        assert_eq!(keys(&specs), ["local", "m:a"]);
        assert_eq!(warn.len(), 1);
    }

    #[test]
    fn validate_machine_rules() {
        let existing = vec![mc("dev", "ssh://dev")];
        assert_eq!(validate_machine("  ", "ssh://x", &existing), Err(MachineError::EmptyName));
        assert_eq!(validate_machine("dev", "ssh://x", &existing), Err(MachineError::DuplicateName));
        assert!(matches!(validate_machine("x", "ssh://", &existing), Err(MachineError::BadEndpoint(_))));
        assert!(matches!(validate_machine("x", "  ", &existing), Err(MachineError::BadEndpoint(_))));
        assert_eq!(validate_machine(" box ", " ssh://box ", &existing), Ok(mc("box", "ssh://box")));
    }

    #[test]
    fn start_prefers_saved_then_first() {
        let s = settings(vec![mc("a", "ssh://a")]);
        let (specs, _) = machine_specs(None, &s, local());
        let saved = WorkspaceState { machine: Some("m:a".into()), session: Some("s9".into()), ..Default::default() };
        assert_eq!(start_target(&specs, &saved, None), ("m:a".into(), Some("s9".into())));
        let gone = WorkspaceState { machine: Some("m:zzz".into()), session: Some("s9".into()), ..Default::default() };
        assert_eq!(start_target(&specs, &gone, None), ("local".into(), None));
        assert_eq!(start_target(&specs, &saved, Some("work")), ("local".into(), Some("work".into())));
        assert_eq!(start_target(&specs, &WorkspaceState::default(), None), ("local".into(), None));
    }


    fn si(id: &str, label: &str) -> SessionInfo {
        SessionInfo { id: id.into(), label: label.into() }
    }

    #[test]
    fn start_choice_rules() {
        let s = vec![si("session:aaaa-111111", "work"), si("session:bbbb-222222", "ai")];
        assert_eq!(start_choice(&s, Some("ai"), true), StartChoice::Open("session:bbbb-222222".into()));
        assert_eq!(start_choice(&s, Some("111111"), false), StartChoice::Open("session:aaaa-111111".into()));
        assert_eq!(start_choice(&s, Some("nope"), true), StartChoice::Missing("nope".into()));
        assert_eq!(start_choice(&s, Some("nope"), false), StartChoice::Open("session:aaaa-111111".into()));
        assert_eq!(start_choice(&s, None, false), StartChoice::Open("session:aaaa-111111".into()));
        assert_eq!(start_choice(&[], None, false), StartChoice::Create);
    }

    #[test]
    fn reconnect_keeps_the_shown_session() {
        let s = vec![si("s1", "a"), si("s2", "b")];
        assert_eq!(after_reconnect(&s, Some("s2")), Some("s2".into()));
        assert_eq!(after_reconnect(&s, Some("gone")), Some("s1".into()));
        assert_eq!(after_reconnect(&[], Some("s2")), None);
    }

    #[test]
    fn saved_target_survives_automatic_choices() {
        let saved = (Some("m:dev"), Some("s9"));
        // Fell back to this computer while dev was unreachable: keep dev.
        assert_eq!(state_target(Some(("local", Some("s1"))), saved, true), (Some("m:dev".into()), Some("s9".into())));
        // Nothing shown yet (closing while connecting): keep dev.
        assert_eq!(state_target(None, saved, false), (Some("m:dev".into()), Some("s9".into())));
        // The user chose: save it.
        assert_eq!(state_target(Some(("local", Some("s1"))), saved, false), (Some("local".into()), Some("s1".into())));
    }

    #[test]
    fn status_line_priority() {
        let notes = vec!["settings broken".to_string()];
        assert_eq!(status_line(None, None, None, &notes).as_deref(), Some("settings broken"));
        assert_eq!(status_line(None, Some("can't open"), None, &notes).as_deref(), Some("can't open · settings broken"));
        assert_eq!(status_line(None, None, Some("reconnecting…"), &[]).as_deref(), Some("reconnecting…"));
        assert_eq!(status_line(Some("Ctrl-b …"), Some("x"), Some("y"), &notes).as_deref(), Some("Ctrl-b …"));
        assert_eq!(status_line(None, None, None, &[]), None);
    }

    #[test]
    fn conn_state_text() {
        assert_eq!(ConnState::Connected.text(), None);
        assert!(ConnState::Failed("no route".into()).text().unwrap().contains("no route"));
        assert!(ConnState::Reconnecting.text().is_some());
    }
}
