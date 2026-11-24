//! The launcher entry and the app icon: on Linux a window's icon comes from the .desktop file
//! named by its app id, so the app writes one (and the icon) under ~/.local/share.

use std::path::{Path, PathBuf};

pub const APP_ID: &str = "dev.rexa.Desktop";

/// rexa's icon, used unless the settings name another.
pub const DEFAULT_ICON: &[u8] = include_bytes!("../assets/rexa.svg");

/// The .desktop file for running `exe`.
pub fn desktop_entry(exe: &Path) -> String {
    let path = exe.to_string_lossy();
    let reserved = |c: char| c.is_whitespace() || "\"'\\><~|&;$*?#()`".contains(c);
    let exec = if path.chars().any(reserved) {
        // Quoted, with " ` $ \ escaped; then the file's own escaping doubles every backslash.
        let mut q = String::from("\"");
        for c in path.chars() {
            if matches!(c, '"' | '`' | '$' | '\\') {
                q.push('\\');
            }
            q.push(c);
        }
        q.push('"');
        q.replace('\\', "\\\\")
    } else {
        path.into_owned()
    };
    format!(
        "[Desktop Entry]\nType=Application\nName=Rexa Desktop\nGenericName=Rex Client\nComment=Terminals, files and transfers on your Rex machines\nExec={exec}\nIcon={APP_ID}\nTerminal=false\nCategories=System;TerminalEmulator;\nKeywords=rex;terminal;sftp;session;claude;\nStartupWMClass={APP_ID}\nStartupNotify=true\n"
    )
}

/// Where the icon goes for `custom` (its kind from the extension), or why it can't be used.
pub fn icon_path(data_home: &Path, custom: Option<&Path>) -> Result<PathBuf, String> {
    let icons = data_home.join("icons/hicolor");
    let Some(c) = custom else { return Ok(icons.join(format!("scalable/apps/{APP_ID}.svg"))) };
    if !c.is_file() {
        return Err(format!("{}: not found", c.display()));
    }
    match c.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).as_deref() {
        Some("svg") => Ok(icons.join(format!("scalable/apps/{APP_ID}.svg"))),
        Some("png") => Ok(icons.join(format!("256x256/apps/{APP_ID}.png"))),
        _ => Err(format!("{}: the icon has to be a PNG or SVG file", c.display())),
    }
}

/// Writes `bytes` to `path` unless it already holds them.
fn put(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if std::fs::read(path).is_ok_and(|b| b == bytes) {
        return Ok(());
    }
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension("rexa-tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(tmp, path)
}

/// Writes the .desktop file and the icon (only what changed); a line for each problem. A
/// custom icon that can't be used falls back to rexa's.
pub fn install(data_home: &Path, exe: &Path, custom: Option<&Path>) -> Vec<String> {
    let mut notes = Vec::new();
    let (target, bytes) = match custom.map(|c| (c, icon_path(data_home, Some(c)))) {
        Some((c, Ok(p))) => match std::fs::read(c) {
            Ok(b) => (p, b),
            Err(e) => {
                notes.push(format!("{}: {e}", c.display()));
                (icon_path(data_home, None).expect("default"), DEFAULT_ICON.to_vec())
            }
        },
        Some((_, Err(e))) => {
            notes.push(e);
            (icon_path(data_home, None).expect("default"), DEFAULT_ICON.to_vec())
        }
        None => (icon_path(data_home, None).expect("default"), DEFAULT_ICON.to_vec()),
    };
    if let Err(e) = put(&target, &bytes) {
        notes.push(format!("{}: {e}", target.display()));
    }
    // The other kind would win or confuse the icon theme: remove it.
    let icons = data_home.join("icons/hicolor");
    for other in [icons.join(format!("scalable/apps/{APP_ID}.svg")), icons.join(format!("256x256/apps/{APP_ID}.png"))] {
        if other != target && other.exists() {
            let _ = std::fs::remove_file(other);
        }
    }
    let entry = data_home.join(format!("applications/{APP_ID}.desktop"));
    if let Err(e) = put(&entry, desktop_entry(exe).as_bytes()) {
        notes.push(format!("{}: {e}", entry.display()));
    }
    notes
}

/// $XDG_DATA_HOME, else ~/.local/share.
pub fn data_home(get: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    get("XDG_DATA_HOME").filter(|d| d.starts_with('/')).map(PathBuf::from).or_else(|| get("HOME").filter(|h| !h.is_empty()).map(|h| Path::new(&h).join(".local/share")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rexa-desktop-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn entry_quotes_the_command() {
        let e = desktop_entry(Path::new("/opt/my apps/rexa-desktop"));
        assert!(e.starts_with("[Desktop Entry]\n"));
        assert!(e.contains("\nExec=\"/opt/my apps/rexa-desktop\"\n"), "{e}");
        assert!(e.contains(&format!("\nIcon={APP_ID}\n")));
        assert!(e.contains(&format!("\nStartupWMClass={APP_ID}\n")));
        assert!(e.contains("Name=Rexa Desktop"));
        let e = desktop_entry(Path::new("/usr/bin/rexa-desktop"));
        assert!(e.contains("\nExec=/usr/bin/rexa-desktop\n"));
        let e = desktop_entry(Path::new("/x/\"$a\\b"));
        assert!(e.contains(r#"Exec="/x/\\"\\$a\\\\b""#), "{e}");
    }

    #[test]
    fn icon_kinds() {
        let h = Path::new("/h");
        assert_eq!(icon_path(h, None).unwrap(), Path::new("/h/icons/hicolor/scalable/apps/dev.rexa.Desktop.svg"));
        let d = tmp("kinds");
        let png = d.join("me.PNG");
        std::fs::write(&png, b"\x89PNG").unwrap();
        assert_eq!(icon_path(h, Some(&png)).unwrap(), Path::new("/h/icons/hicolor/256x256/apps/dev.rexa.Desktop.png"));
        assert!(icon_path(h, Some(&d.join("gone.png"))).unwrap_err().contains("gone.png"));
        let jpg = d.join("me.jpg");
        std::fs::write(&jpg, b"x").unwrap();
        assert!(icon_path(h, Some(&jpg)).is_err());
    }

    #[test]
    fn install_writes_and_switches() {
        let h = tmp("install");
        let exe = Path::new("/usr/bin/rexa-desktop");
        assert!(install(&h, exe, None).is_empty());
        let svg = h.join("icons/hicolor/scalable/apps/dev.rexa.Desktop.svg");
        assert_eq!(std::fs::read(&svg).unwrap(), DEFAULT_ICON);
        let desktop = h.join("applications/dev.rexa.Desktop.desktop");
        assert_eq!(std::fs::read_to_string(&desktop).unwrap(), desktop_entry(exe));
        // A PNG replaces the SVG (no stale icon of the other kind is left).
        let png = h.join("mine.png");
        std::fs::write(&png, b"\x89PNG data").unwrap();
        assert!(install(&h, exe, Some(&png)).is_empty());
        assert!(!svg.exists());
        assert_eq!(std::fs::read(h.join("icons/hicolor/256x256/apps/dev.rexa.Desktop.png")).unwrap(), b"\x89PNG data");
        // A missing one says so and goes back to rexa's.
        let notes = install(&h, exe, Some(&h.join("nope.svg")));
        assert_eq!(notes.len(), 1);
        assert!(svg.exists());
    }
}
