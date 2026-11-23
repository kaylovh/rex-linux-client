//! GUI shortcuts (Ctrl-Shift on Linux, Cmd on macOS) as the same commands as rexa's prefix keys.

use gpui_kit::Keystroke;
use rex_client::prefix::{Command, Dir};

/// The command a shortcut stands for, if any.
pub fn gui_command(ks: &Keystroke) -> Option<Command> {
    let m = &ks.modifiers;
    let main = if cfg!(target_os = "macos") {
        m.platform && !m.control && !m.alt
    } else {
        m.control && m.shift && !m.alt && !m.platform
    };
    if main {
        return Some(match ks.key.as_str() {
            "t" => Command::NewTab,
            "w" => Command::ClosePane,
            "d" => Command::SplitRight,
            "e" => Command::SplitDown,
            "b" => Command::ToggleSidebar,
            "enter" => Command::Zoom,
            "p" => Command::Palette,
            "f" => Command::Search,
            "o" => Command::ToggleFiles,
            "," => Command::Settings,
            _ => return None,
        });
    }
    if m.control && !m.shift && !m.alt && !m.platform {
        return match ks.key.as_str() {
            "pagedown" => Some(Command::NextTab),
            "pageup" => Some(Command::PrevTab),
            _ => None,
        };
    }
    if m.alt && !m.control && !m.shift && !m.platform {
        return Some(Command::Focus(match ks.key.as_str() {
            "left" => Dir::Left,
            "right" => Dir::Right,
            "up" => Dir::Up,
            "down" => Dir::Down,
            _ => return None,
        }));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(s: &str) -> Option<Command> {
        let s: &'static str = s.to_string().leak();
        gui_command(&Keystroke::parse(s).map_err(|e| e.to_string()).unwrap())
    }

    #[test]
    fn shortcuts() {
        let main = if cfg!(target_os = "macos") { "cmd-" } else { "ctrl-shift-" };
        assert_eq!(cmd(&format!("{main}t")), Some(Command::NewTab));
        assert_eq!(cmd(&format!("{main}w")), Some(Command::ClosePane));
        assert_eq!(cmd(&format!("{main}d")), Some(Command::SplitRight));
        assert_eq!(cmd(&format!("{main}e")), Some(Command::SplitDown));
        assert_eq!(cmd(&format!("{main}b")), Some(Command::ToggleSidebar));
        assert_eq!(cmd(&format!("{main}enter")), Some(Command::Zoom));
        assert_eq!(cmd(&format!("{main}p")), Some(Command::Palette));
        assert_eq!(cmd(&format!("{main}f")), Some(Command::Search));
        assert_eq!(cmd(&format!("{main}o")), Some(Command::ToggleFiles));
        let settings = if cfg!(target_os = "macos") { "cmd-," } else { "ctrl-shift-," };
        assert_eq!(cmd(settings), Some(Command::Settings));
        assert_eq!(cmd("ctrl-pagedown"), Some(Command::NextTab));
        assert_eq!(cmd("ctrl-pageup"), Some(Command::PrevTab));
        assert_eq!(cmd("alt-left"), Some(Command::Focus(Dir::Left)));
        assert_eq!(cmd("alt-down"), Some(Command::Focus(Dir::Down)));
    }

    #[test]
    fn plain_keys_are_not_shortcuts() {
        assert_eq!(cmd("t"), None);
        assert_eq!(cmd("ctrl-t"), None);
        assert_eq!(cmd("ctrl-c"), None);
        assert_eq!(cmd("left"), None);
    }
}
