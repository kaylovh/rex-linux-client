//! Previewing a file: highlighted read-only text, an image, or what it is when it can't be shown.

use std::sync::Arc;

use files::FileSystem;
use files::save::{Eol, InPlaceReason, SaveError, Saved, Version, create_new, detect_eol, safe_save, save_in_place, with_eol};
use gpui_cn::{Button, ButtonSize};
use gpui_kit::base::input::InputEvent;
use gpui_kit::component::input::{Editor, EditorState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::base::Disableable as _;
use gpui_kit::{
    App, Focusable as _, PromptButton, PromptLevel, Subscription,
    AnyElement, AppContext as _, Context, Entity, EventEmitter, Image, ImageFormat, InteractiveElement as _, IntoElement, ObjectFit, ParentElement as _,
    Render, SharedString, StatefulInteractiveElement as _, Styled as _, StyledImage as _, Window, div, img, px, rgb,
};

use crate::config::SavedPreview;
use crate::i18n::t;
use crate::i18n::t as t_;

/// The largest file that can be edited.
pub const EDIT_MAX: u64 = 2 * 1024 * 1024;

/// Why a file can't be edited here, or None when it can.
pub fn edit_block_reason(source: Option<&str>, size: u64, utf8: bool) -> Option<String> {
    if source.is_none() {
        return Some(t("Read-only: reached through Rex.", "只读:经 Rex 访问。").into());
    }
    if size > EDIT_MAX {
        return Some(t("Too large to edit (over 2 MB).", "文件太大,不能编辑(超过 2 MB)。").into());
    }
    if !utf8 {
        return Some(t("Not UTF-8: can't be edited.", "不是 UTF-8:不能编辑。").into());
    }
    None
}

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

/// A text tab: shown read-only until Edit; saved safely.
struct TextTab {
    editor: Entity<EditorState>,
    note: Option<String>,
    /// Why it can't be edited, if it can't.
    block: Option<String>,
    editing: bool,
    dirty: bool,
    /// The file as it was when read or last saved (the "changed on disk" check).
    base: Option<Version>,
    eol: Eol,
    /// The text last read or saved, in LF.
    saved: String,
    saving: bool,
    message: Option<String>,
    _sub: Subscription,
}

enum Content {
    Loading,
    Text(Box<TextTab>),
    Image(Arc<Image>),
    Info(String),
}

struct Tab {
    fs: Arc<dyn FileSystem>,
    /// Not on disk yet: created by its first save.
    new_file: bool,
    machine: String,
    /// How the file was read (see locate::source_tag); None: through Rex.
    source: Option<String>,
    path: String,
    name: String,
    content: Content,
}

/// What happens once the pending saves are done.
#[derive(Clone, PartialEq)]
enum AfterSave {
    Nothing,
    Close(String, Option<String>, String),
    Quit,
}

pub enum PreviewEvent {
    /// Every unsaved file was saved for a quit: go on quitting.
    AllSaved,
    /// A tab was closed (the list of open files changed).
    Closed,
    /// A file was saved (its folder may show it now).
    Saved,
}

/// Previews of opened files, one tab each.
pub struct PreviewPanel {
    tabs: Vec<Tab>,
    active: usize,
    after_save: AfterSave,
}

impl EventEmitter<PreviewEvent> for PreviewPanel {}

impl PreviewPanel {
    pub fn new() -> Self {
        PreviewPanel { tabs: Vec::new(), active: 0, after_save: AfterSave::Nothing }
    }

    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    /// The open files that can be read again later, in tab order.
    pub fn files(&self) -> Vec<SavedPreview> {
        self.tabs
            .iter()
            .filter_map(|t| Some(SavedPreview { machine: t.machine.clone(), path: t.path.clone(), source: t.source.clone()? }))
            .collect()
    }

    /// Shows `path` of `machine` (a new tab, or the one already open), read through `fs`.
    pub fn open(&mut self, machine: String, source: Option<String>, path: String, fs: Arc<dyn FileSystem>, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(i) = self.tabs.iter().position(|t| t.machine == machine && t.source == source && t.path == path) {
            self.active = i;
            cx.notify();
            return;
        }
        let name = path.rsplit('/').next().unwrap_or(&path).to_string();
        self.tabs.push(Tab {
            fs: fs.clone(),
            new_file: false,
            machine: machine.clone(),
            source: source.clone(),
            path: path.clone(),
            name: name.clone(),
            content: Content::Loading,
        });
        self.active = self.tabs.len() - 1;
        cx.notify();
        let p = path.clone();
        cx.spawn_in(window, async move |this, cx| {
            let read = cx
                .background_spawn(async move {
                    // Only regular files: opening a pipe or a device would block.
                    let entry = fs.stat_target(&p)?;
                    if entry.kind != files::Kind::File {
                        return Ok((entry, Vec::new()));
                    }
                    let size = entry.size.unwrap_or(0);
                    let bytes = if size > PREVIEW_MAX { Vec::new() } else { fs.read(&p, PREVIEW_MAX + 1)? };
                    Ok::<_, files::FsError>((entry, bytes))
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                let Some(tab) = this.tabs.iter_mut().find(|t| t.machine == machine && t.source == source && t.path == path) else { return };
                tab.content = match read {
                    Err(e) => Content::Info(crate::file_panel::fs_error_text(&e)),
                    Ok((entry, _)) if entry.kind != files::Kind::File => Content::Info(t("Not a regular file: nothing to preview.", "不是普通文件:没有可预览的内容。").into()),
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
                                let note = (!utf8).then(|| t("Not UTF-8: shown as best it can be; it can't be edited.", "不是 UTF-8:尽量显示,不能编辑。").to_string());
                                let block = edit_block_reason(source.as_deref(), size, utf8);
                                Content::Text(Box::new(text_tab(text, language, note, block, Some(Version::read(&entry, &bytes)), window, cx)))
                            }
                        }
                    }
                };
                cx.notify();
            });
        })
        .detach();
    }

    /// A new, empty file in an editor; its first save creates it.
    pub fn open_new(&mut self, machine: String, source: Option<String>, path: String, fs: Arc<dyn FileSystem>, window: &mut Window, cx: &mut Context<Self>) {
        let name = path.rsplit('/').next().unwrap_or(&path).to_string();
        let mut text = text_tab(String::new(), language_for(&name), None, None, None, window, cx);
        text.editing = true;
        let editor = text.editor.clone();
        self.tabs.push(Tab { fs, new_file: true, machine, source, path, name, content: Content::Text(Box::new(text)) });
        self.active = self.tabs.len() - 1;
        editor.update(cx, |e, cx| e.focus(window, cx));
        cx.notify();
    }

    fn text_mut(&mut self, i: usize) -> Option<&mut TextTab> {
        match &mut self.tabs.get_mut(i)?.content {
            Content::Text(t) => Some(t),
            _ => None,
        }
    }

    /// The index of the tab with this identity (tabs may move while a save runs).
    fn find(&self, machine: &str, source: &Option<String>, path: &str) -> Option<usize> {
        self.tabs.iter().position(|t| t.machine == machine && &t.source == source && t.path == path)
    }

    fn is_dirty(&self, i: usize) -> bool {
        self.tabs.get(i).is_some_and(|t| matches!(&t.content, Content::Text(x) if x.dirty || (t.new_file && x.editing)))
    }

    /// The names of the files with unsaved changes.
    pub fn dirty_names(&self) -> Vec<String> {
        (0..self.tabs.len()).filter(|i| self.is_dirty(*i)).map(|i| self.tabs[i].name.clone()).collect()
    }

    /// Saves every unsaved file, then reports AllSaved (a failed or questioned save stops it).
    pub fn save_all_then_quit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dirty: Vec<usize> = (0..self.tabs.len()).filter(|i| self.is_dirty(*i)).collect();
        if dirty.is_empty() {
            cx.emit(PreviewEvent::AllSaved);
            return;
        }
        self.after_save = AfterSave::Quit;
        for i in dirty {
            self.save(i, false, window, cx);
        }
    }

    /// Closes tab `i`, asking first when it has unsaved changes.
    fn request_close(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_dirty(i) {
            self.close(i, cx);
            return;
        }
        let tab = &self.tabs[i];
        let key = (tab.machine.clone(), tab.source.clone(), tab.path.clone());
        let answer = window.prompt(
            PromptLevel::Warning,
            t_("Save changes before closing?", "关闭前保存修改吗?"),
            Some(&tab.name),
            &[
                PromptButton::Ok(t_("Save", "保存").into()),
                PromptButton::Other(t_("Discard", "不保存").into()),
                PromptButton::Cancel(t_("Cancel", "取消").into()),
            ],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let choice = answer.await;
            let _ = this.update_in(cx, |this, window, cx| {
                let Some(i) = this.find(&key.0, &key.1, &key.2) else { return };
                match choice {
                    Ok(0) => {
                        this.after_save = AfterSave::Close(key.0, key.1, key.2);
                        this.save(i, false, window, cx);
                    }
                    Ok(1) => this.close(i, cx),
                    _ => {}
                }
            });
        })
        .detach();
    }

    /// Whether the editor of the shown tab has the keyboard.
    pub fn editor_focused(&self, window: &Window, cx: &App) -> bool {
        // A read-only preview leaves the window's shortcuts alone.
        match self.tabs.get(self.active).map(|t| &t.content) {
            Some(Content::Text(t)) => t.editing && t.editor.read(cx).focus_handle(cx).is_focused(window),
            _ => false,
        }
    }

    pub fn edit(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(t) = self.text_mut(i)
            && t.block.is_none()
        {
            t.editing = true;
            let editor = t.editor.clone();
            editor.update(cx, |e, cx| e.focus(window, cx));
            cx.notify();
        }
    }

    /// Saves the shown tab, if it is being edited.
    pub fn save_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.save(self.active, false, window, cx);
    }

    /// Saves tab `i`: safely, after checking the file didn't change since it was read
    /// (`force`: don't check); asks when it did, or when only an in-place write can keep it.
    fn save(&mut self, i: usize, force: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(i) else { return };
        let (fs, machine, source, path, new_file) = (tab.fs.clone(), tab.machine.clone(), tab.source.clone(), tab.path.clone(), tab.new_file);
        let Some(text) = self.text_mut(i) else { return };
        // Nothing to write: no edits (a new file is written even empty).
        if !text.editing || text.saving || !(text.dirty || new_file || force) {
            return;
        }
        text.saving = true;
        text.message = None;
        let lf = text.editor.read(cx).value().to_string();
        let bytes = with_eol(&lf, text.eol).into_bytes();
        let expected = if force { None } else { text.base.clone() };
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let p = path.clone();
            let result = cx
                .background_spawn(async move {
                    // A new file never replaces one made meanwhile (unless Overwrite was chosen).
                    if new_file && !force { create_new(&*fs, &p, &bytes) } else { safe_save(&*fs, &p, &bytes, expected.as_ref()) }
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| this.saved(&machine, &source, &path, lf, force, result, window, cx));
        })
        .detach();
    }

    /// What a save came back with.
    #[allow(clippy::too_many_arguments)]
    fn saved(
        &mut self,
        machine: &str,
        source: &Option<String>,
        path: &str,
        lf: String,
        force: bool,
        result: Result<Saved, SaveError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(i) = self.find(machine, source, path) else { return };
        if let Some(t) = self.text_mut(i) {
            t.saving = false;
        }
        // Anything but a clean save stops a pending close or quit: the user decides again.
        if result.is_err() {
            self.after_save = AfterSave::Nothing;
        }
        match result {
            Ok(s) => {
                let tab = &mut self.tabs[i];
                tab.new_file = false;
                if let Content::Text(t) = &mut tab.content {
                    t.base = Some(s.version);
                    t.dirty = t.editor.read(cx).value() != lf.as_str();
                    t.saved = lf;
                    t.message = s.fallback_note.then(|| t_("Saved by deleting and renaming: the server can't rename over a file.", "已通过删除再改名保存:服务器不支持直接改名覆盖。").into());
                }
                cx.emit(PreviewEvent::Saved);
                match self.after_save.clone() {
                    AfterSave::Close(m, s, p) if m == machine && &s == source && p == path => {
                        self.after_save = AfterSave::Nothing;
                        // Typed into while it saved: ask again rather than lose that.
                        if let Some(i) = self.find(&m, &s, &p)
                            && !self.is_dirty(i)
                        {
                            self.close(i, cx);
                        }
                    }
                    AfterSave::Quit if self.dirty_names().is_empty() => {
                        self.after_save = AfterSave::Nothing;
                        cx.emit(PreviewEvent::AllSaved);
                    }
                    // Every save is back and something is still unsaved: the quit is off.
                    AfterSave::Quit if !self.tabs.iter().any(|t| matches!(&t.content, Content::Text(x) if x.saving)) => {
                        self.after_save = AfterSave::Nothing;
                    }
                    _ => {}
                }
            }
            Err(SaveError::Changed { .. }) => {
                let answer = window.prompt(
                    PromptLevel::Warning,
                    t_("The file changed on disk since you opened it.", "文件在你打开之后被修改过。"),
                    Some(path),
                    &[
                        PromptButton::Ok(t_("Overwrite", "覆盖").into()),
                        PromptButton::Other(t_("Reload", "重新载入").into()),
                        PromptButton::Cancel(t_("Cancel", "取消").into()),
                    ],
                    cx,
                );
                let (m, s, p) = (machine.to_string(), source.clone(), path.to_string());
                cx.spawn_in(window, async move |this, cx| {
                    let choice = answer.await;
                    let _ = this.update_in(cx, |this, window, cx| {
                        let Some(i) = this.find(&m, &s, &p) else { return };
                        match choice {
                            Ok(0) => {
                                // Overwrite: the file as it is now is the one to replace.
                                if let Some(t) = this.text_mut(i) {
                                    t.dirty = true;
                                }
                                this.save(i, true, window, cx);
                            }
                            Ok(1) => this.reload(i, window, cx),
                            _ => {}
                        }
                    });
                })
                .detach();
            }
            Err(SaveError::NeedsInPlace(reason)) => {
                let why = match reason {
                    InPlaceReason::FolderNotWritable => t_("The folder can't be written, so the file can't be replaced safely.", "所在文件夹不可写,无法安全地替换文件。"),
                    InPlaceReason::OtherOwner => t_("The file belongs to another user or group; replacing it would change that.", "文件属于别的用户或组,替换会改变归属。"),
                    InPlaceReason::HardLinks => t_("The file has other hard links; replacing it would split them.", "文件有其他硬链接,替换会让它们分开。"),
                };
                let detail = format!("{why}\n{}", t_("Write over the file directly? A copy of its current contents is kept on this computer first.", "直接覆盖写入?写入前会先在本机保存一份当前内容的备份。"));
                let answer = window.prompt(
                    PromptLevel::Warning,
                    t_("Write in place?", "直接覆盖写入?"),
                    Some(&detail),
                    &[PromptButton::Ok(t_("Write in place", "直接写入").into()), PromptButton::Cancel(t_("Cancel", "取消").into())],
                    cx,
                );
                let (m, s, p) = (machine.to_string(), source.clone(), path.to_string());
                cx.spawn_in(window, async move |this, cx| {
                    if answer.await != Ok(0) {
                        return;
                    }
                    let _ = this.update_in(cx, |this, window, cx| {
                        let Some(i) = this.find(&m, &s, &p) else { return };
                        this.save_in_place(i, force, window, cx);
                    });
                })
                .detach();
            }
            Err(SaveError::InPlaceFailed { error, restored, backup }) => {
                if let Some(t) = self.text_mut(i) {
                    let how = if restored {
                        t_("Not saved; the file was put back as it was.", "未保存;文件已恢复原样。")
                    } else {
                        t_("Not saved, and the file couldn't be put back — restore it from the backup.", "未保存,且无法恢复原文件——请用备份恢复。")
                    };
                    t.message = Some(format!(
                        "{how} {} {}: {}",
                        crate::file_panel::fs_error_text(&error),
                        t_("Backup", "备份"),
                        backup.display()
                    ));
                }
            }
            Err(SaveError::Fs(e)) => {
                if let Some(t) = self.text_mut(i) {
                    t.message = Some(format!("{}{}", t_("Not saved: ", "未保存:"), crate::file_panel::fs_error_text(&e)));
                }
            }
        }
        cx.notify();
    }

    fn save_in_place(&mut self, i: usize, force: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(i) else { return };
        let (fs, machine, source, path) = (tab.fs.clone(), tab.machine.clone(), tab.source.clone(), tab.path.clone());
        let Some(backups) = crate::config::backup_dir(|k| std::env::var(k).ok()) else {
            if let Some(t) = self.text_mut(i) {
                t.message = Some(t_("Not saved: no folder for the backup (HOME isn't set).", "未保存:没有放备份的文件夹(未设置 HOME)。").into());
            }
            cx.notify();
            return;
        };
        let Some(text) = self.text_mut(i) else { return };
        text.saving = true;
        let lf = text.editor.read(cx).value().to_string();
        let bytes = with_eol(&lf, text.eol).into_bytes();
        // After Overwrite, the file as it is now is the one replaced.
        let expected = if force { None } else { text.base.clone() };
        cx.spawn_in(window, async move |this, cx| {
            let m2 = machine.clone();
            let p2 = path.clone();
            let result = cx.background_spawn(async move { save_in_place(&*fs, &p2, &bytes, expected.as_ref(), &backups, &m2) }).await;
            let _ = this.update_in(cx, |this, window, cx| this.saved(&machine, &source, &path, lf, force, result, window, cx));
        })
        .detach();
    }

    /// Reads the file again into tab `i` (dropping edits).
    fn reload(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(i) else { return };
        let (fs, machine, source, path) = (tab.fs.clone(), tab.machine.clone(), tab.source.clone(), tab.path.clone());
        cx.spawn_in(window, async move |this, cx| {
            let p = path.clone();
            let read = cx
                .background_spawn(async move {
                    let e = fs.stat_target(&p)?;
                    let bytes = fs.read(&p, EDIT_MAX + 1)?;
                    Ok::<_, files::FsError>((Version::read(&e, &bytes), e.size.unwrap_or(0).max(bytes.len() as u64), bytes))
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                let Some(i) = this.find(&machine, &source, &path) else { return };
                let source_tag = this.tabs[i].source.clone();
                let Some(t) = this.text_mut(i) else { return };
                match read {
                    Ok((v, size, bytes)) => {
                        // It may have grown past the edit limit or stopped being UTF-8.
                        let utf8 = std::str::from_utf8(&bytes).is_ok();
                        t.block = edit_block_reason(source_tag.as_deref(), size, utf8);
                        if t.block.is_some() {
                            t.editing = false;
                        }
                        let raw = String::from_utf8_lossy(&bytes).into_owned();
                        t.eol = detect_eol(&raw);
                        let lf = raw.replace("\r\n", "\n");
                        t.saved = lf.clone();
                        t.base = Some(v);
                        t.dirty = false;
                        t.message = None;
                        t.editor.update(cx, |e, cx| e.set_value(lf, window, cx));
                    }
                    Err(e) => t.message = Some(crate::file_panel::fs_error_text(&e)),
                }
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

/// A text tab around `raw` (the editor works in LF; the file's line breaks are kept for saving).
#[allow(clippy::too_many_arguments)]
fn text_tab(
    raw: String,
    language: Option<&'static str>,
    note: Option<String>,
    block: Option<String>,
    base: Option<Version>,
    window: &mut Window,
    cx: &mut Context<PreviewPanel>,
) -> TextTab {
    let eol = detect_eol(&raw);
    let lf = raw.replace("\r\n", "\n");
    let editor = cx.new(|cx| {
        let mut s = EditorState::new(window, cx).line_number(true).searchable(true);
        if let Some(l) = language {
            s = s.language(l);
        }
        s.set_value(lf.clone(), window, cx);
        s
    });
    let sub = cx.subscribe(&editor, |this, editor, ev: &InputEvent, cx| {
        if let InputEvent::Change = ev {
            for tab in &mut this.tabs {
                if let Content::Text(t) = &mut tab.content
                    && &t.editor == &editor
                {
                    t.dirty = t.editor.read(cx).value() != t.saved.as_str();
                }
            }
            cx.notify();
        }
    });
    TextTab { editor, note, block, editing: false, dirty: false, base, eol, saved: lf, saving: false, message: None, _sub: sub }
}

/// The unsaved files for a prompt: up to five names, then how many more.
pub fn unsaved_summary(names: &[String]) -> String {
    let shown = names.iter().take(5).cloned().collect::<Vec<_>>().join(", ");
    if names.len() > 5 { format!("{shown} (+{})", names.len() - 5) } else { shown }
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
                    .child(if matches!(&tab.content, Content::Text(t) if t.dirty) { format!("● {}", tab.name) } else { tab.name.clone() })
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
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.request_close(i, window, cx);
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
            Some(Content::Text(tt)) => {
                let i = self.active;
                let new_file = self.tabs.get(i).is_some_and(|t| t.new_file);
                let main = if cfg!(target_os = "macos") { "Cmd-S" } else { "Ctrl-S" };
                let mut bar = div().flex().flex_wrap().items_center().gap_2().px_2().py_0p5().text_sm();
                if let Some(n) = &tt.note {
                    bar = bar.child(div().text_color(rgb(0xe0a060)).child(n.clone()));
                }
                if tt.editing {
                    let label = if tt.saving { t("Saving…", "正在保存…") } else { t("Save", "保存") };
                    bar = bar
                        .child(
                            Button::new("pv-save")
                                .primary()
                                .size(ButtonSize::Xs)
                                .label(label)
                                .disabled(tt.saving || !(tt.dirty || new_file))
                                .on_click(cx.listener(move |this, _, window, cx| this.save(i, false, window, cx))),
                        )
                        .child(div().text_color(rgb(0x8a8a8a)).child(main));
                } else if let Some(why) = &tt.block {
                    bar = bar.child(div().text_color(rgb(0x8a8a8a)).child(why.clone()));
                } else {
                    bar = bar.child(
                        Button::new("pv-edit").size(ButtonSize::Xs).label(t("Edit", "编辑")).on_click(cx.listener(move |this, _, window, cx| this.edit(i, window, cx))),
                    );
                }
                if let Some(m) = &tt.message {
                    bar = bar.child(div().text_color(rgb(0xe0a060)).child(m.clone()));
                }
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .child(bar)
                    .child(div().flex_1().min_h_0().child(Editor::new(&tt.editor).readonly(!tt.editing).h_full()))
                    .into_any_element()
            }
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
    fn editable_rules() {
        let _lang = crate::i18n::TEST_LANG.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::i18n::set_language(crate::config::Language::En);
        assert_eq!(edit_block_reason(Some("local"), 100, true), None);
        assert_eq!(edit_block_reason(Some("sftp:ssh://me@dev"), EDIT_MAX, true), None);
        assert!(edit_block_reason(None, 100, true).unwrap().contains("Rex"));
        assert!(edit_block_reason(Some("local"), EDIT_MAX + 1, true).unwrap().contains("2 MB"));
        assert!(edit_block_reason(Some("local"), 100, false).unwrap().contains("UTF-8"));
    }

    #[test]
    fn unsaved_summary_lists_names() {
        assert_eq!(unsaved_summary(&["a.txt".into()]), "a.txt");
        assert_eq!(unsaved_summary(&["a".into(), "b".into(), "c".into()]), "a, b, c");
        let many: Vec<String> = (1..=7).map(|i| format!("f{i}")).collect();
        assert!(unsaved_summary(&many).starts_with("f1, f2, f3, f4, f5"));
        assert!(unsaved_summary(&many).contains('2'));
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
