//! What the app remembers between runs: settings (language, machines, terminal font) and the
//! workspace (which machine and session were shown, the sidebar, the window size), as JSON
//! files in the config directory, each written to a temporary file and renamed into place.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    #[default]
    Auto,
    En,
    Zh,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineConfig {
    pub name: String,
    pub endpoint: String,
}

/// Which of a server's own Rex hosts the sidebar shows (a server with no entry: all for
/// this computer, none for a remote one).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShownHosts {
    /// The machine key of the server listing them.
    pub server: String,
    /// Their labels.
    pub hosts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FontSettings {
    pub family: Option<String>,
    pub size: Option<f32>,
    pub fallbacks: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub language: Language,
    pub machines: Vec<MachineConfig>,
    pub font: FontSettings,
    pub shown_hosts: Vec<ShownHosts>,
}

/// The file panel as it was left.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FilesState {
    pub open: bool,
    /// Pinned to (machine key, folder); None: following the terminal.
    pub pinned: Option<(String, String)>,
    pub show_hidden: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkspaceState {
    pub machine: Option<String>,
    pub session: Option<String>,
    pub sidebar_open: bool,
    pub sidebar_width: Option<f32>,
    pub window_size: Option<(f32, f32)>,
    pub files: FilesState,
    /// Open previews as (machine key, path).
    pub previews: Vec<(String, String)>,
}

impl Default for FontSettings {
    fn default() -> Self {
        let fallbacks: &[&str] = if cfg!(target_os = "macos") {
            &["Apple Color Emoji", "Symbols Nerd Font Mono", "PingFang SC"]
        } else {
            &["Noto Color Emoji", "Symbols Nerd Font Mono", "Noto Sans Mono CJK SC"]
        };
        Self { family: None, size: None, fallbacks: fallbacks.iter().map(|s| s.to_string()).collect() }
    }
}

impl Default for WorkspaceState {
    fn default() -> Self {
        Self { machine: None, session: None, sidebar_open: true, sidebar_width: None, window_size: None, files: FilesState::default(), previews: Vec::new() }
    }
}

const SETTINGS: &str = "settings.json";
const STATE: &str = "state.json";

/// Where the files live: $XDG_CONFIG_HOME/rexa-desktop, else the platform's config folder
/// under $HOME.
pub fn config_dir(get: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let nonempty = |k: &str| get(k).filter(|v| !v.is_empty());
    if let Some(x) = nonempty("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(x).join("rexa-desktop"));
    }
    let home = PathBuf::from(nonempty("HOME")?);
    Some(if cfg!(target_os = "macos") {
        home.join("Library/Application Support/rexa-desktop")
    } else {
        home.join(".config/rexa-desktop")
    })
}

/// The settings, or the defaults with the reason when the file can't be used. A missing file
/// is not a problem.
pub fn load_settings(dir: &Path) -> (Settings, Option<String>) {
    match fs::read(dir.join(SETTINGS)) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => (Settings::default(), None),
        Err(e) => (Settings::default(), Some(e.to_string())),
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(s) => (s, None),
            Err(e) => (Settings::default(), Some(e.to_string())),
        },
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, bytes).and_then(|_| fs::rename(&tmp, path)).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

/// Saves the settings. An existing file that isn't valid settings is the user's: it is only
/// replaced with \`replace_broken\`, and then kept as settings.json.bak.
pub fn save_settings(dir: &Path, s: &Settings, replace_broken: bool) -> io::Result<()> {
    let path = dir.join(SETTINGS);
    if let Ok(old) = fs::read(&path)
        && serde_json::from_slice::<Settings>(&old).is_err()
    {
        if !replace_broken {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "settings.json is not valid; not replacing it"));
        }
        fs::rename(&path, dir.join("settings.json.bak"))?;
    }
    let mut bytes = serde_json::to_vec_pretty(s).map_err(io::Error::other)?;
    bytes.push(b'\n');
    write_atomic(&path, &bytes)
}

/// The saved workspace; anything unreadable is the default.
pub fn load_state(dir: &Path) -> WorkspaceState {
    fs::read(dir.join(STATE)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub fn save_state(dir: &Path, s: &WorkspaceState) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(s).map_err(io::Error::other)?;
    write_atomic(&dir.join(STATE), &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rexa-desktop-cfg-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn settings_round_trip() {
        let d = tmp("round");
        let s = Settings {
            language: Language::Zh,
            machines: vec![MachineConfig { name: "dev".into(), endpoint: "ssh://dev".into() }],
            font: FontSettings { family: Some("Iosevka".into()), size: Some(15.0), fallbacks: vec!["Noto Color Emoji".into()] },
            shown_hosts: vec![ShownHosts { server: "local".into(), hosts: vec!["studio".into()] }],
        };
        save_settings(&d, &s, false).unwrap();
        assert_eq!(load_settings(&d), (s, None));
    }

    #[test]
    fn missing_file_is_defaults() {
        let d = tmp("missing");
        assert_eq!(load_settings(&d), (Settings::default(), None));
        assert!(load_state(&d).sidebar_open);
        assert!(!Settings::default().font.fallbacks.is_empty());
    }

    #[test]
    fn broken_file_is_kept() {
        let d = tmp("broken");
        let bad = br#"{"language": 5"#;
        fs::write(d.join("settings.json"), bad).unwrap();
        let (s, why) = load_settings(&d);
        assert_eq!(s, Settings::default());
        assert!(why.is_some());
        assert!(save_settings(&d, &s, false).is_err());
        assert_eq!(fs::read(d.join("settings.json")).unwrap(), bad);
        save_settings(&d, &s, true).unwrap();
        assert_eq!(fs::read(d.join("settings.json.bak")).unwrap(), bad);
        assert_eq!(load_settings(&d), (s, None));
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let d = tmp("unknown");
        fs::write(d.join("settings.json"), r#"{"language":"zh","extra":1}"#).unwrap();
        let (s, why) = load_settings(&d);
        assert_eq!(s.language, Language::Zh);
        assert_eq!(why, None);
    }

    #[test]
    fn no_tmp_left_behind() {
        let d = tmp("tmpfiles");
        save_settings(&d, &Settings::default(), false).unwrap();
        save_state(&d, &WorkspaceState { machine: Some("local".into()), session: Some("s1".into()), ..Default::default() }).unwrap();
        let names: Vec<String> = fs::read_dir(&d).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert!(names.iter().all(|n| !n.ends_with(".tmp")), "{names:?}");
        assert_eq!(load_state(&d).session.as_deref(), Some("s1"));
    }

    #[test]
    fn files_state_round_trip() {
        let d = tmp("filesstate");
        let s = WorkspaceState {
            files: FilesState { open: true, pinned: Some(("m:dev".into(), "/srv/app".into())), show_hidden: true },
            previews: vec![("local".into(), "/etc/hosts".into())],
            ..Default::default()
        };
        save_state(&d, &s).unwrap();
        assert_eq!(load_state(&d), s);
        // An old state file without them loads with the panel closed.
        fs::write(d.join("state.json"), r#"{"machine":"local"}"#).unwrap();
        assert_eq!(load_state(&d).files, FilesState::default());
    }

    #[test]
    fn state_with_bad_file_is_default() {
        let d = tmp("badstate");
        fs::write(d.join("state.json"), "nonsense").unwrap();
        assert_eq!(load_state(&d), WorkspaceState::default());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn config_dir_prefers_xdg() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
        };
        assert_eq!(config_dir(env(&[("XDG_CONFIG_HOME", "/x"), ("HOME", "/h")])), Some(PathBuf::from("/x/rexa-desktop")));
        assert_eq!(config_dir(env(&[("HOME", "/h")])), Some(PathBuf::from("/h/.config/rexa-desktop")));
        assert_eq!(config_dir(env(&[("XDG_CONFIG_HOME", ""), ("HOME", "/h")])), Some(PathBuf::from("/h/.config/rexa-desktop")));
        assert_eq!(config_dir(env(&[])), None);
    }
}
