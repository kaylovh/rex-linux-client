# Changes to the Nerd Fonts symbols font

`SymbolsNerdFontMono-Regular.ttf` is Nerd Fonts 3.5.1 "Symbols Nerd Font Mono" (MIT, see
LICENSE-NerdFontsSymbolsOnly) with one change: the letter `m` maps to a new, empty glyph
(`rexa_m`). GPUI on Linux skips any font without an `m`, so without it the font couldn't be
a fallback font and icons were drawn from unrelated system fonts.

`SymbolsNerdFontMono-Bold.ttf` is that same file marked bold (weight 700, Bold style names),
so bold terminal text finds the icons as well. The glyphs are unchanged.
