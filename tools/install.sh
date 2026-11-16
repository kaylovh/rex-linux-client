#!/bin/sh
# Build rexa (optimized) and install it for this user:
#   ~/.local/bin/rexa            the client (run it in any terminal)
#   ~/.local/bin/rexa-app        rexa in a window of its own
#   ~/.local/share/applications  the "Rexa" app in your launcher, with its icon
# Debug builds of libghostty are far too slow for real use; always ReleaseFast.
set -eu
cd "$(dirname "$0")/.."
tools/zig build -Doptimize=ReleaseFast
bin="$HOME/.local/bin"
data="${XDG_DATA_HOME:-$HOME/.local/share}"
mkdir -p "$bin" "$data/applications" "$data/icons/hicolor/scalable/apps"
install -m 0755 zig-out/bin/rexa "$bin/rexa"
install -m 0755 tools/rexa-app "$bin/rexa-app"
install -m 0644 assets/rexa.svg "$data/icons/hicolor/scalable/apps/rexa.svg"
sed "s|@BIN@|$bin|g" assets/rexa.desktop > "$data/applications/dev.rexa.Rexa.desktop"
chmod 0644 "$data/applications/dev.rexa.Rexa.desktop"
command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database -q "$data/applications" || true
command -v gtk-update-icon-cache >/dev/null 2>&1 && gtk-update-icon-cache -q -t "$data/icons/hicolor" 2>/dev/null || true
echo "installed $bin/rexa, $bin/rexa-app and the Rexa app (launcher entry + icon)"
case ":$PATH:" in
  *":$bin:"*) ;;
  *) echo "note: $bin is not on your PATH" ;;
esac
other=$(command -v -a rexa 2>/dev/null | grep -v "^$bin/rexa$" | head -1 || true)
[ -n "$other" ] && echo "note: another rexa is at $other (check which one comes first on PATH)"
exit 0
