//! One language for the whole UI: Chinese when the locale says so, English otherwise.

use std::sync::OnceLock;

/// Whether the locale (LC_ALL, then LC_MESSAGES, then LANG; the first that is set) is Chinese.
pub fn zh_from(get: impl Fn(&str) -> Option<String>) -> bool {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .find_map(|k| get(k).filter(|v| !v.is_empty()))
        .is_some_and(|v| v.starts_with("zh"))
}

pub fn zh() -> bool {
    static ZH: OnceLock<bool> = OnceLock::new();
    *ZH.get_or_init(|| zh_from(|k| std::env::var(k).ok()))
}

pub fn t(en: &'static str, zh_text: &'static str) -> &'static str {
    if zh() { zh_text } else { en }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
    }

    #[test]
    fn picks_chinese_from_lang() {
        assert!(zh_from(env(&[("LANG", "zh_CN.UTF-8")])));
        assert!(!zh_from(env(&[("LANG", "en_US.UTF-8")])));
        assert!(!zh_from(env(&[("LC_ALL", "C"), ("LANG", "zh_CN.UTF-8")])));
        assert!(zh_from(env(&[("LC_ALL", ""), ("LC_MESSAGES", "zh_TW"), ("LANG", "en_US")])));
        assert!(!zh_from(env(&[])));
    }
}
