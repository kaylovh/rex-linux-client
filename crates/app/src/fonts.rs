//! Fonts shipped inside the app, so terminal icons render without installing anything.

use std::borrow::Cow;

use gpui_kit::App;

/// (family, file): the Nerd Font symbols (MIT, assets/fonts/LICENSE-NerdFontsSymbolsOnly), changed
/// as assets/fonts/CHANGES.md says: a blank 'm' so GPUI loads it as a fallback, and the same
/// glyphs marked bold so bold text finds its icons too.
pub const EMBEDDED: &[(&str, &[u8])] = &[
    ("Symbols Nerd Font Mono", include_bytes!("../assets/fonts/SymbolsNerdFontMono-Regular.ttf")),
    ("Symbols Nerd Font Mono", include_bytes!("../assets/fonts/SymbolsNerdFontMono-Bold.ttf")),
];

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

    /// GPUI on Linux only loads a family whose font maps 'm' (it measures with it): without it
    /// the symbols font drops out of the fallback chain and icons come from whatever system
    /// font has something at that code point (DejaVu Sans draws U+F43A as a Greek letter).
    #[test]
    fn symbols_font_can_be_a_fallback() {
        let mut weights = Vec::new();
        for (_, bytes) in EMBEDDED {
            let face = ttf_parser::Face::parse(bytes, 0).unwrap();
            assert!(face.glyph_index('m').is_some(), "no 'm': GPUI skips the font");
            for icon in ['\u{f43a}', '\u{f07c}', '\u{e711}', '\u{e0b4}', '\u{f02a2}'] {
                assert!(face.glyph_index(icon).is_some(), "{:x} missing", icon as u32);
            }
            weights.push(face.weight().to_number());
        }
        // Bold text looks for a bold face of the fallback family.
        assert_eq!(weights, [400, 700]);
    }
}
