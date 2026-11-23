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

/// A Ghostty theme text for the terminal engine (its only public way in).
fn ghostty_text(t: &TermColors) -> String {
    let mut s = format!(
        "background = #{:06x}\nforeground = #{:06x}\ncursor-color = #{:06x}\nselection-background = #{:06x}\nselection-foreground = #{:06x}\n",
        t.background, t.foreground, t.cursor, t.selection, t.foreground
    );
    for (i, c) in t.palette.iter().enumerate() {
        s.push_str(&format!("palette = {i}=#{c:06x}\n"));
    }
    s
}

pub fn terminal_colors(t: &AppTheme) -> gpui_cn::terminal::TerminalColors {
    gpui_cn::terminal::TerminalColors::from_ghostty_theme(&ghostty_text(&t.terminal)).unwrap_or_default()
}

/// The theme in use, for code that draws (read without a context: one window, one thread).
static CURRENT: std::sync::RwLock<Option<AppTheme>> = std::sync::RwLock::new(None);

/// The theme in use.
pub fn current() -> AppTheme {
    CURRENT.read().ok().and_then(|c| c.clone()).unwrap_or_else(|| find(&builtin(), None).clone())
}

/// The interface colors in use.
pub fn ui() -> UiColors {
    CURRENT.read().ok().and_then(|c| c.as_ref().map(|t| t.ui)).unwrap_or_else(|| find(&builtin(), None).ui)
}

fn hsla(c: Rgb) -> gpui_kit::Hsla {
    gpui_kit::rgb(c).into()
}

fn hex(c: Rgb) -> String {
    format!("#{c:06x}")
}

/// Makes `t` the theme of every part of the window: gpui-cn's components (sidebar, tabs,
/// dialogs), gpui-component's (editor, inputs) and the app's own drawing. Terminals take
/// [`terminal_colors`] from their owner.
pub fn apply(t: &AppTheme, cx: &mut gpui_kit::App) {
    if let Ok(mut c) = CURRENT.write() {
        *c = Some(t.clone());
    }
    let ui = t.ui;
    let dark = t.dark;
    gpui_cn::Theme::update(cx, |th| {
        let mut cfg = if dark { gpui_cn::ThemeConfig::dark() } else { gpui_cn::ThemeConfig::light() };
        cfg.surface = hsla(ui.background);
        cfg.ink = hsla(ui.text);
        cfg.accent = hsla(ui.accent);
        cfg.semantic.success = hsla(ui.success);
        cfg.semantic.destructive = hsla(ui.error);
        cfg.semantic.warning = hsla(ui.warning);
        if dark {
            th.dark = cfg;
            th.mode = gpui_cn::ThemeMode::Dark;
        } else {
            th.light = cfg;
            th.mode = gpui_cn::ThemeMode::Light;
        }
    });
    use gpui_kit::component::{Theme, ThemeMode};
    Theme::change(if dark { ThemeMode::Dark } else { ThemeMode::Light }, None, cx);
    let config = serde_json::json!({
        "name": t.name,
        "mode": if dark { "dark" } else { "light" },
        "colors": {
            "background": hex(ui.background),
            "foreground": hex(ui.text),
            "border": hex(ui.border),
            "input.border": hex(ui.border),
            "muted.background": hex(ui.panel),
            "muted.foreground": hex(ui.muted),
            "accent.background": hex(ui.hover),
            "accent.foreground": hex(ui.text),
            "primary.background": hex(ui.accent),
            "popover.background": hex(ui.elevated),
            "popover.foreground": hex(ui.text),
            "selection.background": hex(ui.selected),
            "list.active.background": hex(ui.selected),
            "list.hover.background": hex(ui.hover),
            "sidebar.background": hex(ui.panel),
            "sidebar.foreground": hex(ui.text),
            "tab_bar.background": hex(ui.panel),
            "title_bar.background": hex(ui.panel),
            "danger.background": hex(ui.error),
            "warning.background": hex(ui.warning),
            "success.background": hex(ui.success),
            "caret": hex(ui.accent),
        }
    });
    if let Ok(cfg) = serde_json::from_value::<gpui_kit::component::ThemeConfig>(config) {
        Theme::global_mut(cx).apply_config(&std::rc::Rc::new(cfg));
    }
    cx.refresh_windows();
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

/// Puts an example theme in `dir` the first time, to copy from.
pub fn write_example(dir: &Path) {
    let p = dir.join("example.json.txt");
    if !p.exists() {
        let _ = std::fs::write(
            p,
            r##"{
  "name": "My Theme",
  "base": "One Dark",
  "dark": true,
  "ui": {
    "background": "#1f2329", "panel": "#1a1d22", "elevated": "#262a31", "hover": "#2c313a",
    "selected": "#3e4451", "border": "#14161a", "text": "#c8ccd4", "muted": "#7f848e",
    "accent": "#e5a050", "warning": "#e5c07b", "error": "#e06c75", "success": "#98c379"
  },
  "terminal": {
    "background": "#1f2329", "foreground": "#c8ccd4", "cursor": "#e5a050", "selection": "#3e4451",
    "palette": ["#1f2329", "#e06c75", "#98c379", "#e5c07b", "#61afef", "#c678dd", "#56b6c2", "#abb2bf",
                "#5c6370", "#e06c75", "#98c379", "#e5c07b", "#61afef", "#c678dd", "#56b6c2", "#ffffff"]
  }
}
Copy this to a .json file in this folder, change what you like (leave out what you don't:
it comes from "base"), then pick it in Settings.
复制成本目录下的 .json 文件,修改需要的颜色(省略的项取自 "base"),然后在设置里选择。
"##,
        );
    }
}

/// Every theme: the built-ins, then the user's in `<config>/themes`; and what couldn't be read.
pub fn load_all(config: Option<&Path>) -> (Vec<AppTheme>, Vec<String>) {
    let mut all = builtin();
    let (mine, errors) = match config {
        Some(c) => load_dir(&c.join("themes"), &all),
        None => (vec![], vec![]),
    };
    for t in mine {
        // A user theme with a built-in's name replaces it.
        all.retain(|b| b.name != t.name);
        all.push(t);
    }
    (all, errors)
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
    fn terminal_colors_follow_the_theme() {
        let b = builtin();
        for t in &b {
            let c = terminal_colors(t);
            let bg = c.background();
            assert_eq!((bg.0 as u32) << 16 | (bg.1 as u32) << 8 | bg.2 as u32, t.terminal.background, "{}", t.name);
        }
    }

    /// Every color in the app comes from the theme.
    #[test]
    fn no_hard_coded_colors() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        for e in std::fs::read_dir(src).unwrap().flatten() {
            let p = e.path();
            if p.file_name().is_some_and(|n| n == "theme.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&p).unwrap();
            assert!(!text.contains("rgb(0x"), "{} has a hard-coded color", p.display());
        }
    }

    #[test]
    fn unknown_name_falls_back() {
        let b = builtin();
        assert_eq!(find(&b, Some("Gone")).name, DEFAULT);
        assert_eq!(find(&b, None).name, DEFAULT);
        assert_eq!(find(&b, Some("Dracula")).name, "Dracula");
    }
}
