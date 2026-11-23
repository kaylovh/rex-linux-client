//! Which machines the window shows: this computer and the ones in the settings, or only the
//! one given with -S; and where to start.

use rex_proto::transport::Endpoint;

use crate::config::{MachineConfig, Settings, WorkspaceState};
use crate::i18n::t;

#[derive(Debug, Clone)]
pub struct MachineSpec {
    /// "local", "cli" or "m:" + name: stable across runs, saved in the workspace state.
    pub key: String,
    pub name: String,
    pub endpoint: Endpoint,
    pub local: bool,
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
        return (vec![MachineSpec { key: "cli".into(), name: text, endpoint, local: false }], Vec::new());
    }
    let mut specs = vec![MachineSpec { key: "local".into(), name: t("This computer", "本机").into(), endpoint: local, local: true }];
    let mut warnings = Vec::new();
    let mut seen: Vec<&MachineConfig> = Vec::new();
    for m in &settings.machines {
        match validate_machine(&m.name, &m.endpoint, &seen.iter().map(|m| (*m).clone()).collect::<Vec<_>>()) {
            Ok(ok) => {
                let endpoint = Endpoint::parse(&ok.endpoint).expect("validated");
                specs.push(MachineSpec { key: format!("m:{}", ok.name), name: ok.name, endpoint, local: false });
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

    #[test]
    fn conn_state_text() {
        assert_eq!(ConnState::Connected.text(), None);
        assert!(ConnState::Failed("no route".into()).text().unwrap().contains("no route"));
        assert!(ConnState::Reconnecting.text().is_some());
    }
}
