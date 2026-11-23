//! The window's colors: one theme for the panels, the dialogs, the editor and the terminals.
//! Built-in themes ship in assets/themes.json; a user adds one as a JSON file in
//! `<config>/themes/`, starting from a built-in (`"base"`) and changing what it lists.

use std::path::Path;

use serde::Deserialize;

/// A color as 0xRRGGBB.
pub type Rgb = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UiColors {
    /// The window and the editor.
    pub background: Rgb,
    /// Docks, the tab strip and the status bar.
    pub panel: Rgb,
    /// Dialogs, menus, floating panes.
    pub elevated: Rgb,
    pub hover: Rgb,
    pub selected: Rgb,
    pub border: Rgb,
    pub text: Rgb,
    pub muted: Rgb,
    pub accent: Rgb,
    pub warning: Rgb,
    pub error: Rgb,
    pub success: Rgb,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TermColors {
    pub background: Rgb,
    pub foreground: Rgb,
    pub cursor: Rgb,
    pub selection: Rgb,
    pub palette: [Rgb; 16],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppTheme {
    pub name: String,
    pub dark: bool,
    pub ui: UiColors,
    pub terminal: TermColors,
}

/// The theme used when none is chosen or the chosen one is gone.
pub const DEFAULT: &str = "One Dark";

/// A theme as written in JSON: every field optional, taken from `base` when left out.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ThemeFile {
    name: String,
    #[serde(default)]
    base: Option<String>,
    #[serde(default)]
    dark: Option<bool>,
    #[serde(default)]
    ui: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    terminal: TermFile,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct TermFile {
    background: Option<String>,
    foreground: Option<String>,
    cursor: Option<String>,
    selection: Option<String>,
    palette: Vec<String>,
}

fn parse_hex(s: &str) -> Result<Rgb, String> {
    let h = s.trim().strip_prefix('#').unwrap_or("");
    if h.len() == 6
        && let Ok(v) = u32::from_str_radix(h, 16)
    {
        return Ok(v);
    }
    Err(format!("{s:?} is not a #rrggbb color"))
}

const BLANK: AppTheme = AppTheme {
    name: String::new(),
    dark: true,
    ui: UiColors { background: 0, panel: 0, elevated: 0, hover: 0, selected: 0, border: 0, text: 0, muted: 0, accent: 0, warning: 0, error: 0, success: 0 },
    terminal: TermColors { background: 0, foreground: 0, cursor: 0, selection: 0, palette: [0; 16] },
};

impl ThemeFile {
    fn build(self, base: &AppTheme) -> Result<AppTheme, String> {
        let mut t = base.clone();
        t.name = self.name;
        if let Some(d) = self.dark {
            t.dark = d;
        }
        for (key, value) in &self.ui {
            let c = parse_hex(value).map_err(|e| format!("ui.{key}: {e}"))?;
            let slot = match key.as_str() {
                "background" => &mut t.ui.background,
                "panel" => &mut t.ui.panel,
                "elevated" => &mut t.ui.elevated,
                "hover" => &mut t.ui.hover,
                "selected" => &mut t.ui.selected,
                "border" => &mut t.ui.border,
                "text" => &mut t.ui.text,
                "muted" => &mut t.ui.muted,
                "accent" => &mut t.ui.accent,
                "warning" => &mut t.ui.warning,
                "error" => &mut t.ui.error,
                "success" => &mut t.ui.success,
                other => return Err(format!("ui.{other}: unknown color")),
            };
            *slot = c;
        }
        let term = self.terminal;
        for (name, value, slot) in [
            ("background", term.background, &mut t.terminal.background),
            ("foreground", term.foreground, &mut t.terminal.foreground),
            ("cursor", term.cursor, &mut t.terminal.cursor),
            ("selection", term.selection, &mut t.terminal.selection),
        ] {
            if let Some(v) = value {
                *slot = parse_hex(&v).map_err(|e| format!("terminal.{name}: {e}"))?;
            }
        }
        if term.palette.len() > 16 {
            return Err("terminal.palette: at most 16 colors".into());
        }
        for (i, v) in term.palette.iter().enumerate() {
            t.terminal.palette[i] = parse_hex(v).map_err(|e| format!("terminal.palette[{i}]: {e}"))?;
        }
        Ok(t)
    }
}

/// The themes that ship with the app.
pub fn builtin() -> Vec<AppTheme> {
    let files: Vec<ThemeFile> = serde_json::from_str(include_str!("../assets/themes.json")).expect("built-in themes");
    files.into_iter().map(|f| f.build(&BLANK).expect("built-in theme")).collect()
}

/// The user's themes in `dir` (*.json), and a line per file that couldn't be read.
pub fn load_dir(dir: &Path, builtin: &[AppTheme]) -> (Vec<AppTheme>, Vec<String>) {
    let (mut themes, mut errors) = (Vec::new(), Vec::new());
    let Ok(entries) = std::fs::read_dir(dir) else { return (themes, errors) };
    let mut paths: Vec<_> = entries.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "json")).collect();
    paths.sort();
    for p in paths {
        let file_name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let parsed = std::fs::read_to_string(&p)
            .map_err(|e| e.to_string())
            .and_then(|s| serde_json::from_str::<ThemeFile>(&s).map_err(|e| e.to_string()))
            .and_then(|f| {
                let base = find(builtin, f.base.as_deref()).clone();
                f.build(&base)
            });
        match parsed {
            Ok(t) => themes.push(t),
            Err(e) => errors.push(format!("{file_name}: {e}")),
        }
    }
    (themes, errors)
}

/// The theme named `name`, else the default.
pub fn find<'a>(all: &'a [AppTheme], name: Option<&str>) -> &'a AppTheme {
    name.and_then(|n| all.iter().find(|t| t.name == n))
        .or_else(|| all.iter().find(|t| t.name == DEFAULT))
        .or_else(|| all.first())
        .expect("at least one theme")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("rexa-theme-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn builtins_are_complete() {
        let all = builtin();
        assert!(all.len() >= 8);
        let mut names: Vec<&str> = all.iter().map(|t| t.name.as_str()).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), all.len());
        assert!(all.iter().any(|t| t.name == DEFAULT && t.dark));
        assert!(all.iter().any(|t| !t.dark), "a light theme");
        // Light themes have dark text.
        for t in all.iter().filter(|t| !t.dark) {
            assert!(t.ui.text < 0x808080, "{}", t.name);
        }
    }

    #[test]
    fn user_theme_changes_its_base() {
        let d = dir("user");
        std::fs::write(
            d.join("mine.json"),
            r##"{"name": "Mine", "base": "Nord", "ui": {"accent": "#ff00aa"}, "terminal": {"background": "#000000", "palette": ["#111111"]}}"##,
        )
        .unwrap();
        let b = builtin();
        let (mine, errors) = load_dir(&d, &b);
        assert!(errors.is_empty(), "{errors:?}");
        let nord = find(&b, Some("Nord"));
        assert_eq!(mine.len(), 1);
        let m = &mine[0];
        assert_eq!(m.name, "Mine");
        assert_eq!(m.ui.accent, 0xff00aa);
        assert_eq!(m.ui.text, nord.ui.text);
        assert_eq!(m.dark, nord.dark);
        assert_eq!(m.terminal.background, 0x000000);
        assert_eq!(m.terminal.palette[0], 0x111111);
        assert_eq!(m.terminal.palette[1], nord.terminal.palette[1]);
    }

    #[test]
    fn bad_files_are_named() {
        let d = dir("bad");
        std::fs::write(d.join("broken.json"), "{ not json").unwrap();
        std::fs::write(d.join("color.json"), r#"{"name": "X", "ui": {"text": "blue"}}"#).unwrap();
        std::fs::write(d.join("notes.txt"), "ignored").unwrap();
        let (themes, errors) = load_dir(&d, &builtin());
        assert!(themes.is_empty());
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(errors.iter().any(|e| e.contains("broken.json")));
        assert!(errors.iter().any(|e| e.contains("color.json") && e.contains("blue")));
        // A missing folder is no error.
        assert_eq!(load_dir(&d.join("nope"), &builtin()), (vec![], vec![]));
    }

    #[test]
    fn unknown_name_falls_back() {
        let b = builtin();
        assert_eq!(find(&b, Some("Gone")).name, DEFAULT);
        assert_eq!(find(&b, None).name, DEFAULT);
        assert_eq!(find(&b, Some("Dracula")).name, "Dracula");
    }
}
