//! One language for the whole UI: the one chosen in the settings, or Chinese when the locale
//! says so and English otherwise.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

use crate::config::Language;

/// Whether the locale (LC_ALL, then LC_MESSAGES, then LANG; the first that is set) is Chinese.
pub fn zh_from(get: impl Fn(&str) -> Option<String>) -> bool {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .find_map(|k| get(k).filter(|v| !v.is_empty()))
        .is_some_and(|v| v.starts_with("zh"))
}

/// 0 follows the locale, 1 English, 2 Chinese.
static CHOSEN: AtomicU8 = AtomicU8::new(0);

pub fn set_language(l: Language) {
    CHOSEN.store(
        match l {
            Language::Auto => 0,
            Language::En => 1,
            Language::Zh => 2,
        },
        Ordering::Relaxed,
    );
}

pub fn zh() -> bool {
    static LOCALE_ZH: OnceLock<bool> = OnceLock::new();
    match CHOSEN.load(Ordering::Relaxed) {
        1 => false,
        2 => true,
        _ => *LOCALE_ZH.get_or_init(|| zh_from(|k| std::env::var(k).ok())),
    }
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
    fn language_override() {
        set_language(Language::Zh);
        assert_eq!(t("a", "乙"), "乙");
        set_language(Language::En);
        assert_eq!(t("a", "乙"), "a");
        set_language(Language::Auto);
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
