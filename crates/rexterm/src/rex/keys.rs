//! GPUI key events (as gpui-cn's `KeyInput`) -> the `key` object of a Rex `client.key`
//! request. The server runs its key bindings and then encodes the key for the terminal's
//! current keyboard mode (legacy, kitty, ...). Mirrors rexa's `keys.zig`.

use gpui_cn::terminal::input::KeyInput;
use serde_json::Value;

use gpui_cn::terminal::input::KeyAction;
use serde_json::json;

/// A printable key's physical name and its unshifted (US layout) character.
fn printable(c: char) -> Option<(String, char)> {
    let lower = c.to_ascii_lowercase();
    if lower.is_ascii_lowercase() {
        return Some((format!("key_{lower}"), lower));
    }
    if c.is_ascii_digit() {
        return Some((format!("digit_{c}"), c));
    }
    let (name, base) = match c {
        '`' | '~' => ("backquote", '`'),
        '\\' | '|' => ("backslash", '\\'),
        '[' | '{' => ("bracket_left", '['),
        ']' | '}' => ("bracket_right", ']'),
        ',' | '<' => ("comma", ','),
        '=' | '+' => ("equal", '='),
        '-' | '_' => ("minus", '-'),
        '.' | '>' => ("period", '.'),
        '\'' | '"' => ("quote", '\''),
        ';' | ':' => ("semicolon", ';'),
        '/' | '?' => ("slash", '/'),
        '!' => ("digit_1", '1'),
        '@' => ("digit_2", '2'),
        '#' => ("digit_3", '3'),
        '$' => ("digit_4", '4'),
        '%' => ("digit_5", '5'),
        '^' => ("digit_6", '6'),
        '&' => ("digit_7", '7'),
        '*' => ("digit_8", '8'),
        '(' => ("digit_9", '9'),
        ')' => ("digit_0", '0'),
        _ => return None,
    };
    Some((name.to_string(), base))
}

/// A named (non-printable) GPUI key: its physical name and unshifted codepoint.
fn named(key: &str) -> Option<(String, u32)> {
    let (name, cp): (&str, u32) = match key {
        "space" => ("space", ' ' as u32),
        "enter" => ("enter", '\r' as u32),
        "tab" => ("tab", '\t' as u32),
        "backspace" => ("backspace", 0x7f),
        "escape" => ("escape", 0x1b),
        "insert" => ("insert", 0),
        "delete" => ("delete", 0),
        "left" => ("arrow_left", 0),
        "right" => ("arrow_right", 0),
        "up" => ("arrow_up", 0),
        "down" => ("arrow_down", 0),
        "pageup" => ("page_up", 0),
        "pagedown" => ("page_down", 0),
        "home" => ("home", 0),
        "end" => ("end", 0),
        "menu" => ("context_menu", 0),
        _ => {
            let n = key.strip_prefix('f')?.parse::<u32>().ok()?;
            return (1..=25).contains(&n).then(|| (format!("f{n}"), 0));
        }
    };
    Some((name.to_string(), cp))
}

pub fn to_rex_key(k: &KeyInput) -> Option<Value> {
    let m = k.modifiers;
    let mut chars = k.key.chars();
    let (name, unshifted, default_text) = match (chars.next(), chars.next()) {
        (Some(c), None) if c != ' ' => {
            let (name, base) = printable(c)?;
            (name, base as u32, Some(c.to_string()))
        }
        _ => {
            let (name, cp) = named(&k.key)?;
            (name, cp, (k.key == "space").then(|| " ".to_string()))
        }
    };
    let text = if m.control || m.super_key {
        String::new()
    } else {
        k.text.clone().or(default_text).unwrap_or_default()
    };
    let mods: Vec<&str> = [(m.shift, "shift"), (m.control, "ctrl"), (m.alt, "alt"), (m.super_key, "super")]
        .into_iter()
        .filter_map(|(on, n)| on.then_some(n))
        .collect();
    let action = match k.action {
        KeyAction::Press => "press",
        KeyAction::Repeat => "repeat",
        KeyAction::Release => "release",
    };
    Some(json!({
        "key": name,
        "mods": mods.join("+"),
        "consumed_mods": if m.shift && !text.is_empty() { "shift" } else { "" },
        "text": text,
        "unshifted_codepoint": unshifted,
        "composing": false,
        "action": action,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_cn::terminal::input::{KeyAction, Modifiers};
    use serde_json::json;

    fn mods(shift: bool, control: bool) -> Modifiers {
        let mut m = Modifiers::default();
        m.shift = shift;
        m.control = control;
        m
    }

    #[test]
    fn shift_a() {
        let k = KeyInput::new("a").with_text("A").with_modifiers(mods(true, false));
        assert_eq!(
            to_rex_key(&k).unwrap(),
            json!({"key":"key_a","mods":"shift","consumed_mods":"shift","text":"A","unshifted_codepoint":97,"composing":false,"action":"press"})
        );
    }

    #[test]
    fn ctrl_c() {
        let k = KeyInput::new("c").with_text("c").with_modifiers(mods(false, true));
        assert_eq!(
            to_rex_key(&k).unwrap(),
            json!({"key":"key_c","mods":"ctrl","consumed_mods":"","text":"","unshifted_codepoint":99,"composing":false,"action":"press"})
        );
    }

    #[test]
    fn shift_enter() {
        let k = KeyInput::new("enter").with_modifiers(mods(true, false));
        let v = to_rex_key(&k).unwrap();
        assert_eq!(v["key"], "enter");
        assert_eq!(v["mods"], "shift");
        assert_eq!(v["unshifted_codepoint"], 13);
    }

    #[test]
    fn arrows_and_f_keys() {
        assert_eq!(to_rex_key(&KeyInput::new("left")).unwrap()["key"], "arrow_left");
        assert_eq!(to_rex_key(&KeyInput::new("pageup")).unwrap()["key"], "page_up");
        assert_eq!(to_rex_key(&KeyInput::new("f5")).unwrap()["key"], "f5");
        let rel = KeyInput::new("up").with_action(KeyAction::Release);
        assert_eq!(to_rex_key(&rel).unwrap()["action"], "release");
    }

    #[test]
    fn percent_maps_to_digit_5() {
        let k = KeyInput::new("%").with_text("%").with_modifiers(mods(true, false));
        let v = to_rex_key(&k).unwrap();
        assert_eq!(v["key"], "digit_5");
        assert_eq!(v["unshifted_codepoint"], 53);
        assert_eq!(v["text"], "%");
    }

    #[test]
    fn unknown_key_is_none() {
        assert!(to_rex_key(&KeyInput::new("ü")).is_none());
    }
}
