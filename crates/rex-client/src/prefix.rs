//! rexa's Ctrl-b prefix keys: after Ctrl-b, one key names a command.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    NewTab,
    NextTab,
    PrevTab,
    /// 0-based
    Tab(u8),
    SplitRight,
    SplitDown,
    Focus(Dir),
    FocusNext,
    Zoom,
    ClosePane,
    RenameTab,
    PrevSession,
    NextSession,
    NewSession,
    RenameSession,
    ToggleSidebar,
    Quit,
    /// Ctrl-b twice: send Ctrl-b to the program.
    SendPrefix,
    Palette,
    Settings,
    CloseSession,
    Search,
    ServerInfo,
    ToggleFiles,
    /// Save the file being edited (not a prefix key).
    Save,
}

/// The command for `key` (a GPUI key name such as "c", "%", "left", "b") pressed after the
/// prefix; `shift` and `ctrl` as held. None: not a prefix key (the prefix just ends).
pub fn prefix_command(key: &str, shift: bool, ctrl: bool) -> Option<Command> {
    use Command::*;
    if ctrl {
        return (key == "b").then_some(SendPrefix);
    }
    // A shifted key may arrive as its symbol or as shift + the base key.
    let key = match (shift, key) {
        (true, "5") => "%",
        (true, "'") => "\"",
        (true, "9") => "(",
        (true, "0") => ")",
        (true, "4") => "$",
        (true, "n") => "N",
        (true, ";") => ":",
        (true, "s") => "S",
        (_, k) => k,
    };
    Some(match key {
        "c" => NewTab,
        "n" => NextTab,
        "p" => PrevTab,
        "%" => SplitRight,
        "\"" => SplitDown,
        "left" => Focus(Dir::Left),
        "right" => Focus(Dir::Right),
        "up" => Focus(Dir::Up),
        "down" => Focus(Dir::Down),
        "o" => FocusNext,
        "z" => Zoom,
        "x" => ClosePane,
        "," => RenameTab,
        "(" => PrevSession,
        ")" => NextSession,
        "N" => NewSession,
        "$" => RenameSession,
        ":" => Palette,
        "/" => Search,
        "S" => ServerInfo,
        "e" => ToggleFiles,
        "s" => ToggleSidebar,
        "d" => Quit,
        k if k.len() == 1 && ("1"..="9").contains(&k) => Tab(k.as_bytes()[0] - b'1'),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use Command::*;

    fn k(key: &str) -> Option<Command> {
        prefix_command(key, false, false)
    }

    #[test]
    fn prefix_table_matches_rexa() {
        assert_eq!(k("c"), Some(NewTab));
        assert_eq!(k("n"), Some(NextTab));
        assert_eq!(k("p"), Some(PrevTab));
        assert_eq!(k("1"), Some(Tab(0)));
        assert_eq!(k("9"), Some(Tab(8)));
        assert_eq!(k("%"), Some(SplitRight));
        assert_eq!(k("\""), Some(SplitDown));
        assert_eq!(k("left"), Some(Focus(Dir::Left)));
        assert_eq!(k("up"), Some(Focus(Dir::Up)));
        assert_eq!(k("o"), Some(FocusNext));
        assert_eq!(k("z"), Some(Zoom));
        assert_eq!(k("x"), Some(ClosePane));
        assert_eq!(k(","), Some(RenameTab));
        assert_eq!(k("("), Some(PrevSession));
        assert_eq!(k(")"), Some(NextSession));
        assert_eq!(k("$"), Some(RenameSession));
        assert_eq!(k("s"), Some(ToggleSidebar));
        assert_eq!(k("d"), Some(Quit));
        assert_eq!(prefix_command("n", true, false), Some(NewSession));
        assert_eq!(prefix_command("N", false, false), Some(NewSession));
        assert_eq!(prefix_command("b", false, true), Some(SendPrefix));
        assert_eq!(k(":"), Some(Palette));
        assert_eq!(k("/"), Some(Search));
        assert_eq!(k("S"), Some(ServerInfo));
        assert_eq!(k("e"), Some(ToggleFiles));
        assert_eq!(prefix_command("s", true, false), Some(ServerInfo));
        assert_eq!(prefix_command(";", true, false), Some(Palette));
    }

    #[test]
    fn unknown_key_after_prefix_cancels() {
        assert_eq!(k("q"), None);
        assert_eq!(k("f5"), None);
        assert_eq!(prefix_command("c", false, true), None);
    }

    #[test]
    fn shifted_symbols() {
        assert_eq!(prefix_command("5", true, false), Some(SplitRight));
        assert_eq!(prefix_command("'", true, false), Some(SplitDown));
        assert_eq!(prefix_command("9", true, false), Some(PrevSession));
        assert_eq!(prefix_command("0", true, false), Some(NextSession));
        assert_eq!(prefix_command("4", true, false), Some(RenameSession));
    }
}
