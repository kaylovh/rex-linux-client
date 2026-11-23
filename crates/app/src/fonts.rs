//! Fonts shipped inside the app, so terminal icons render without installing anything.

use std::borrow::Cow;

use gpui_kit::App;

/// (family, file): the Nerd Font symbols (MIT, assets/fonts/LICENSE-NerdFontsSymbolsOnly).
pub const EMBEDDED: &[(&str, &[u8])] = &[("Symbols Nerd Font Mono", include_bytes!("../assets/fonts/SymbolsNerdFontMono-Regular.ttf"))];

/// Makes the embedded fonts available by family name (the default fallbacks name them).
pub fn register(cx: &App) {
    let fonts = EMBEDDED.iter().map(|(_, bytes)| Cow::Borrowed(*bytes)).collect();
    if let Err(e) = cx.text_system().add_fonts(fonts) {
        eprintln!("rexa-desktop: can't load the bundled fonts: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_fonts_are_fonts() {
        assert!(EMBEDDED.iter().any(|(family, _)| *family == "Symbols Nerd Font Mono"));
        for (family, bytes) in EMBEDDED {
            assert!(bytes.len() > 1000, "{family} is empty");
            assert!(bytes.starts_with(&[0, 1, 0, 0]) || bytes.starts_with(b"OTTO"), "{family} is not a TrueType/OpenType font");
        }
    }
}
