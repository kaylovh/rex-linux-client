//! Previewing a file: highlighted read-only text, an image, or what it is when it can't be shown.

use std::sync::Arc;

use files::FileSystem;
use gpui_kit::component::input::{Editor, EditorState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, EventEmitter, Image, ImageFormat, InteractiveElement as _, IntoElement, ObjectFit, ParentElement as _,
    Render, SharedString, StatefulInteractiveElement as _, Styled as _, StyledImage as _, Window, div, img, px, rgb,
};

use crate::i18n::t;

/// The most a preview reads.
pub const PREVIEW_MAX: u64 = 5 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewKind {
    Text { language: Option<&'static str>, utf8: bool },
    Image,
    Binary,
    TooLarge,
}

fn ext(name: &str) -> Option<String> {
    let (stem, e) = name.rsplit_once('.')?;
    (!stem.is_empty()).then(|| e.to_ascii_lowercase())
}

pub fn image_format(name: &str) -> Option<ImageFormat> {
    Some(match ext(name)?.as_str() {
        "png" => ImageFormat::Png,
        "jpg" | "jpeg" => ImageFormat::Jpeg,
        "gif" => ImageFormat::Gif,
        "webp" => ImageFormat::Webp,
        "bmp" => ImageFormat::Bmp,
        "svg" => ImageFormat::Svg,
        _ => return None,
    })
}

/// What a preview of `name` can show, from its first bytes and its size.
pub fn classify(name: &str, bytes: &[u8], size: u64) -> PreviewKind {
    if size > PREVIEW_MAX {
        return PreviewKind::TooLarge;
    }
    if image_format(name).is_some() {
        return PreviewKind::Image;
    }
    if bytes.iter().take(8192).any(|b| *b == 0) {
        return PreviewKind::Binary;
    }
    PreviewKind::Text { language: language_for(name), utf8: std::str::from_utf8(bytes).is_ok() }
}

/// The highlighter for a file name, if one fits.
pub fn language_for(name: &str) -> Option<&'static str> {
    let lower = name.to_ascii_lowercase();
    if lower == ".env" || lower.starts_with(".env.") || matches!(lower.as_str(), ".bashrc" | ".zshrc" | ".profile" | ".bash_profile") {
        return Some("bash");
    }
    if matches!(lower.as_str(), "makefile" | "gnumakefile") {
        return Some("make");
    }
    Some(match ext(name)?.as_str() {
        "rs" => "rust",
        "py" | "pyi" => "python",
        "js" | "mjs" | "cjs" | "jsx" => "javascript",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "tsx",
        "json" | "jsonc" => "json",
        "yml" | "yaml" => "yaml",
        "toml" => "toml",
        "sh" | "bash" | "zsh" => "bash",
        "go" => "go",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => "cpp",
        "md" | "markdown" => "markdown",
        "html" | "htm" => "html",
        "css" => "css",
        "sql" => "sql",
        "java" => "java",
        "lua" => "lua",
        "zig" => "zig",
        "mk" => "make",
        _ => return None,
    })
}

enum Content {
    Loading,
    Text { editor: Entity<EditorState>, note: Option<String> },
    Image(Arc<Image>),
    Info(String),
}

struct Tab {
    machine: String,
    path: String,
    name: String,
    content: Content,
}

pub enum PreviewEvent {
    /// A tab was closed (the list of open files changed).
    Closed,
}

/// Previews of opened files, one tab each.
pub struct PreviewPanel {
    tabs: Vec<Tab>,
    active: usize,
}

impl EventEmitter<PreviewEvent> for PreviewPanel {}

impl PreviewPanel {
    pub fn new() -> Self {
        PreviewPanel { tabs: Vec::new(), active: 0 }
    }

    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    /// The open files as (machine, path), in tab order.
    pub fn files(&self) -> Vec<(String, String)> {
        self.tabs.iter().map(|t| (t.machine.clone(), t.path.clone())).collect()
    }

    /// Shows `path` of `machine` (a new tab, or the one already open), read through `fs`.
    pub fn open(&mut self, machine: String, path: String, fs: Arc<dyn FileSystem>, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(i) = self.tabs.iter().position(|t| t.machine == machine && t.path == path) {
            self.active = i;
            cx.notify();
            return;
        }
        let name = path.rsplit('/').next().unwrap_or(&path).to_string();
        self.tabs.push(Tab { machine: machine.clone(), path: path.clone(), name: name.clone(), content: Content::Loading });
        self.active = self.tabs.len() - 1;
        cx.notify();
        let p = path.clone();
        cx.spawn_in(window, async move |this, cx| {
            let read = cx
                .background_spawn(async move {
                    let entry = fs.stat(&p)?;
                    let size = entry.size.unwrap_or(0);
                    let bytes = if size > PREVIEW_MAX { Vec::new() } else { fs.read(&p, PREVIEW_MAX + 1)? };
                    Ok::<_, files::FsError>((entry, bytes))
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                let Some(tab) = this.tabs.iter_mut().find(|t| t.machine == machine && t.path == path) else { return };
                tab.content = match read {
                    Err(e) => Content::Info(crate::file_panel::fs_error_text(&e)),
                    Ok((entry, bytes)) => {
                        let size = entry.size.unwrap_or(bytes.len() as u64).max(bytes.len() as u64);
                        match classify(&name, &bytes, size) {
                            PreviewKind::TooLarge => Content::Info(format!("{} ({})", t("Too large to preview", "文件太大,不预览"), human_size(size))),
                            PreviewKind::Binary => Content::Info(format!("{} ({})", t("Binary file", "二进制文件"), human_size(size))),
                            PreviewKind::Image => match image_format(&name) {
                                Some(f) => Content::Image(Arc::new(Image::from_bytes(f, bytes))),
                                None => Content::Info(t("Can't show this image.", "无法显示这张图片。").into()),
                            },
                            PreviewKind::Text { language, utf8 } => {
                                let text = String::from_utf8_lossy(&bytes).into_owned();
                                let editor = cx.new(|cx| {
                                    let mut s = EditorState::new(window, cx).line_number(true).searchable(true);
                                    if let Some(l) = language {
                                        s = s.language(l);
                                    }
                                    s.set_value(text, window, cx);
                                    s
                                });
                                let note = (!utf8).then(|| t("Not UTF-8: shown as best it can be; it can't be edited.", "不是 UTF-8:尽量显示,不能编辑。").to_string());
                                Content::Text { editor, note }
                            }
                        }
                    }
                };
                cx.notify();
            });
        })
        .detach();
    }

    fn close(&mut self, i: usize, cx: &mut Context<Self>) {
        if i < self.tabs.len() {
            self.tabs.remove(i);
        }
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len().saturating_sub(1);
        }
        cx.emit(PreviewEvent::Closed);
        cx.notify();
    }
}

pub fn human_size(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 { format!("{n} B") } else { format!("{v:.1} {}", UNITS[u]) }
}

impl Render for PreviewPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut strip = div().flex().flex_none().h(px(30.)).overflow_hidden().border_b_1().border_color(rgb(0x2a2a2a));
        for (i, tab) in self.tabs.iter().enumerate() {
            let active = i == self.active;
            strip = strip.child(
                div()
                    .id(SharedString::from(format!("pv-tab-{i}")))
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .text_sm()
                    .cursor_pointer()
                    .when(active, |d| d.bg(rgb(0x2a2a2a)))
                    .child(tab.name.clone())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.active = i;
                        cx.notify();
                    }))
                    .child(
                        div()
                            .id(SharedString::from(format!("pv-close-{i}")))
                            .px_1()
                            .text_color(rgb(0x8a8a8a))
                            .hover(|s| s.text_color(rgb(0xe0e0e0)))
                            .child("×")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.close(i, cx);
                            })),
                    ),
            );
        }
        let body: AnyElement = match self.tabs.get(self.active).map(|t| &t.content) {
            None => div().into_any_element(),
            Some(Content::Loading) => div().p_3().text_color(rgb(0x9a9a9a)).child(t("Loading…", "正在读取…")).into_any_element(),
            Some(Content::Info(text)) => div().p_3().text_color(rgb(0xb0b0b0)).child(text.clone()).into_any_element(),
            Some(Content::Image(image)) => div()
                .size_full()
                .p_2()
                .flex()
                .items_center()
                .justify_center()
                .child(img(image.clone()).max_w_full().max_h_full().object_fit(ObjectFit::Contain))
                .into_any_element(),
            Some(Content::Text { editor, note }) => div()
                .size_full()
                .flex()
                .flex_col()
                .children(note.clone().map(|n| div().px_2().py_1().text_sm().text_color(rgb(0xe0a060)).child(n)))
                .child(div().flex_1().min_h_0().child(Editor::new(editor).readonly(true).h_full()))
                .into_any_element(),
        };
        let path = self.tabs.get(self.active).map(|t| t.path.clone()).unwrap_or_default();
        div()
            .size_full()
            .flex()
            .flex_col()
            .text_color(rgb(0xd6d6d6))
            .child(strip)
            .child(div().px_2().py_0p5().text_xs().text_color(rgb(0x8a8a8a)).overflow_hidden().whitespace_nowrap().child(path))
            .child(div().flex_1().min_h_0().child(body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_rules() {
        assert_eq!(classify("a.rs", b"fn main() {}", 12), PreviewKind::Text { language: Some("rust"), utf8: true });
        assert_eq!(classify("big.log", b"x", PREVIEW_MAX + 1), PreviewKind::TooLarge);
        assert_eq!(classify("a.bin", b"ab\0cd", 5), PreviewKind::Binary);
        assert_eq!(classify("pic.PNG", b"\x89PNG\r\n\x1a\n\0\0", 10), PreviewKind::Image);
        assert_eq!(classify("photo.jpeg", b"\xff\xd8\xff", 3), PreviewKind::Image);
        assert_eq!(classify("latin1.txt", b"caf\xe9", 4), PreviewKind::Text { language: None, utf8: false });
        // An image too big to preview is still too big.
        assert_eq!(classify("huge.png", b"", PREVIEW_MAX + 1), PreviewKind::TooLarge);
    }

    #[test]
    fn language_for_names() {
        assert_eq!(language_for("main.rs"), Some("rust"));
        assert_eq!(language_for("x.py"), Some("python"));
        assert_eq!(language_for("App.tsx"), Some("tsx"));
        assert_eq!(language_for("a.ts"), Some("typescript"));
        assert_eq!(language_for("package.json"), Some("json"));
        assert_eq!(language_for("ci.yml"), Some("yaml"));
        assert_eq!(language_for("Cargo.toml"), Some("toml"));
        assert_eq!(language_for(".env"), Some("bash"));
        assert_eq!(language_for(".env.local"), Some("bash"));
        assert_eq!(language_for("run.sh"), Some("bash"));
        assert_eq!(language_for("Makefile"), Some("make"));
        assert_eq!(language_for("README.md"), Some("markdown"));
        assert_eq!(language_for("notes.txt"), None);
        assert_eq!(language_for("LICENSE"), None);
    }
}
