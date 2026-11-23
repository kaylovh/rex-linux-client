//! The file panel: a tree of one machine's files beside the terminals, following the focused
//! terminal unless pinned, with new / rename / delete / copy path / cd here.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use files::{FileEntry, FileSystem, FsError, Kind, Listing, join, parent};
use gpui_cn::{Button, ButtonSize, Input};
use gpui_kit::assets::IconName;
use gpui_kit::base::input::{InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::base::Disableable as _;
use gpui_kit::{
    App, AppContext as _, Focusable as _, ClipboardItem, Context, Div, Entity, EventEmitter, InteractiveElement as _, IntoElement, ParentElement as _, PromptButton,
    PromptLevel, Render, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, px, rgb, uniform_list,
};

use crate::i18n::t;

/// One visible line of the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub path: String,
    pub name: String,
    pub depth: usize,
    pub dir: bool,
    pub expanded: bool,
}

/// Folders first, then by name ignoring case; hidden names (a leading dot) only when asked.
pub fn sort_entries(entries: &[FileEntry], show_hidden: bool) -> Vec<FileEntry> {
    let mut v: Vec<FileEntry> = entries.iter().filter(|e| show_hidden || !e.name.starts_with('.')).cloned().collect();
    v.sort_by(|a, b| b.is_dir().cmp(&a.is_dir()).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())).then_with(|| a.name.cmp(&b.name)));
    v
}

/// The rows to show under `root`: its entries, and the entries of every expanded folder
/// right under it, deeper by one.
pub fn rows_for_tree(root: &str, children: &HashMap<String, Vec<FileEntry>>, expanded: &HashSet<String>, show_hidden: bool) -> Vec<Row> {
    fn walk(dir: &str, depth: usize, children: &HashMap<String, Vec<FileEntry>>, expanded: &HashSet<String>, show_hidden: bool, out: &mut Vec<Row>) {
        let Some(entries) = children.get(dir) else { return };
        for e in sort_entries(entries, show_hidden) {
            let path = join(dir, &e.name);
            let open = e.is_dir() && expanded.contains(&path);
            out.push(Row { path: path.clone(), name: e.name.clone(), depth, dir: e.is_dir(), expanded: open });
            if open {
                walk(&path, depth + 1, children, expanded, show_hidden, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, 0, children, expanded, show_hidden, &mut out);
    out
}

/// The entry at `path` from its folder's last listing, shown or not.
pub fn lookup<'a>(children: &'a HashMap<String, Vec<FileEntry>>, path: &str) -> Option<&'a FileEntry> {
    let dir = parent(path)?;
    let name = path.trim_end_matches('/').rsplit('/').next()?;
    children.get(&dir)?.iter().find(|e| e.name == name)
}

/// What the panel asks the workspace for.
pub enum PanelEvent {
    OpenFile { path: String },
    /// Refresh with nothing to refresh (the connection failed): find the place again.
    Retry,
    CdHere { path: String },
    PickMachine(String),
    Follow,
}

#[derive(Clone)]
enum Prompt {
    NewFile,
    NewDir,
    Rename(String),
}

/// The most rows one folder shows.
const ROW_CAP: usize = 2000;

pub struct FilePanel {
    machine: Option<String>,
    machine_name: String,
    fs: Option<Arc<dyn FileSystem>>,
    read_only: Option<String>,
    root: Option<String>,
    children: HashMap<String, Vec<FileEntry>>,
    capped: HashSet<String>,
    expanded: HashSet<String>,
    selected: Option<String>,
    show_hidden: bool,
    pinned: bool,
    message: Option<String>,
    unavailable: Option<String>,
    path_input: Entity<InputState>,
    prompt: Option<(Prompt, Entity<InputState>, Subscription)>,
    picker_open: bool,
    machines: Vec<(String, String)>,
    /// Bumped on every new location: answers for an older one are dropped.
    generation: u64,
    /// The path field shows the new top folder on the next render.
    sync_path_input: bool,
    /// The folder last asked for (refreshed when its listing failed).
    wanted: Option<String>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<PanelEvent> for FilePanel {}

/// An error in the panel's language.
pub fn fs_error_text(e: &FsError) -> String {
    match e {
        FsError::NotFound => t("Not found.", "找不到。").into(),
        FsError::Exists => t("Something with that name is already there.", "已有同名文件或文件夹。").into(),
        FsError::Denied => t("Permission denied.", "没有权限。").into(),
        FsError::NotDir => t("Not a folder.", "不是文件夹。").into(),
        FsError::ReadOnly(_) => t("Read-only: files here are reached through Rex, which can only read them.", "只读:这台机器的文件经 Rex 访问,只能读取。").into(),
        FsError::Gone(why) => format!("{}{why}", t("Connection lost: ", "连接断开:")),
        FsError::Other(why) => why.clone(),
    }
}

impl FilePanel {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let path_input = cx.new(|cx| InputState::new(window, cx).placeholder(t("Path", "路径")));
        let sub = cx.subscribe_in(&path_input, window, |this, input, ev: &InputEvent, _, cx| {
            if let InputEvent::PressEnter { .. } = ev {
                let path = input.read(cx).value().trim().to_string();
                if !path.is_empty() {
                    this.go(Some(path), cx);
                }
            }
        });
        FilePanel {
            machine: None,
            machine_name: String::new(),
            fs: None,
            read_only: None,
            root: None,
            children: HashMap::new(),
            capped: HashSet::new(),
            expanded: HashSet::new(),
            selected: None,
            show_hidden: false,
            pinned: false,
            message: None,
            unavailable: None,
            path_input,
            prompt: None,
            picker_open: false,
            machines: Vec::new(),
            generation: 0,
            sync_path_input: false,
            wanted: None,
            _subs: vec![sub],
        }
    }

    /// The file system shown now.
    pub fn fs(&self) -> Option<Arc<dyn FileSystem>> {
        self.fs.clone()
    }
    pub fn machine(&self) -> Option<&str> {
        self.machine.as_deref()
    }
    pub fn root(&self) -> Option<&str> {
        self.root.as_deref()
    }
    pub fn pinned(&self) -> bool {
        self.pinned
    }
    pub fn show_hidden(&self) -> bool {
        self.show_hidden
    }
    pub fn set_pinned(&mut self, pinned: bool, cx: &mut Context<Self>) {
        self.pinned = pinned;
        cx.notify();
    }
    pub fn set_show_hidden(&mut self, on: bool, cx: &mut Context<Self>) {
        self.show_hidden = on;
        cx.notify();
    }
    pub fn set_machines(&mut self, machines: Vec<(String, String)>, cx: &mut Context<Self>) {
        self.machines = machines;
        cx.notify();
    }

    /// Whether one of the panel's text fields has the keyboard (the workspace then leaves keys alone).
    pub fn typing(&self, window: &Window, cx: &App) -> bool {
        self.path_input.read(cx).focus_handle(cx).is_focused(window)
            || self.prompt.as_ref().is_some_and(|(_, i, _)| i.read(cx).focus_handle(cx).is_focused(window))
    }

    /// Shows another machine (or the same at another folder). `dir` None: its home.
    pub fn set_location(
        &mut self,
        machine: String,
        name: String,
        fs: Result<Arc<dyn FileSystem>, String>,
        dir: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let same_fs = match (&self.fs, &fs) {
            (Some(a), Ok(b)) => Arc::ptr_eq(a, b),
            _ => false,
        };
        if same_fs && self.machine.as_deref() == Some(machine.as_str()) && dir.is_some() && dir.as_deref() == self.root.as_deref() {
            return;
        }
        if !same_fs || self.machine.as_deref() != Some(machine.as_str()) {
            self.children.clear();
            self.capped.clear();
            self.expanded.clear();
            self.selected = None;
            self.root = None;
        }
        self.machine = Some(machine);
        self.machine_name = name;
        self.message = None;
        self.prompt = None;
        match fs {
            Ok(fs) => {
                self.read_only = fs.writable().err();
                self.fs = Some(fs);
                self.unavailable = None;
                self.go(dir, cx);
            }
            Err(why) => {
                self.fs = None;
                self.unavailable = Some(why);
                cx.notify();
            }
        }
    }

    /// Lists `dir` (None: home) and shows it as the top of the tree.
    pub fn go(&mut self, dir: Option<String>, cx: &mut Context<Self>) {
        self.wanted = dir.clone();
        let Some(fs) = self.fs.clone() else { return };
        self.generation += 1;
        let generation = self.generation;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let dir = match dir {
                        Some(d) => d,
                        None => fs.home()?,
                    };
                    fs.list(&dir)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                match result {
                    Ok(listing) => {
                        let root = listing.path.clone();
                        this.store(listing);
                        if this.root.as_deref() != Some(root.as_str()) {
                            this.expanded.clear();
                            this.selected = None;
                        }
                        this.root = Some(root);
                        this.message = None;
                        this.sync_path_input = true;
                    }
                    Err(e) => this.message = Some(fs_error_text(&e)),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn store(&mut self, mut listing: Listing) {
        if listing.entries.len() > ROW_CAP || listing.truncated {
            listing.entries.truncate(ROW_CAP);
            self.capped.insert(listing.path.clone());
        } else {
            self.capped.remove(&listing.path);
        }
        self.children.insert(listing.path, listing.entries);
    }

    /// Loads one folder's entries (a folder opened in the tree, or one changed).
    fn load(&mut self, dir: String, cx: &mut Context<Self>) {
        let Some(fs) = self.fs.clone() else { return };
        let generation = self.generation;
        cx.spawn(async move |this, cx| {
            let d = dir.clone();
            let result = cx.background_spawn(async move { fs.list(&d) }).await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                match result {
                    Ok(mut listing) => {
                        // Keyed by the path asked for, so the tree finds it.
                        listing.path = dir;
                        this.store(listing);
                    }
                    Err(e) => {
                        this.expanded.remove(&dir);
                        this.message = Some(fs_error_text(&e));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.fs.is_none() {
            cx.emit(PanelEvent::Retry);
            return;
        }
        let Some(root) = self.root.clone() else {
            self.go(self.wanted.clone(), cx);
            return;
        };
        self.load(root, cx);
        for d in self.expanded.clone() {
            self.load(d, cx);
        }
    }

    fn toggle(&mut self, path: String, cx: &mut Context<Self>) {
        if !self.expanded.remove(&path) {
            self.expanded.insert(path.clone());
            self.load(path, cx);
        }
        cx.notify();
    }

    fn click(&mut self, row: &Row, cx: &mut Context<Self>) {
        self.selected = Some(row.path.clone());
        if row.dir {
            self.toggle(row.path.clone(), cx);
        } else {
            cx.emit(PanelEvent::OpenFile { path: row.path.clone() });
        }
        cx.notify();
    }

    /// The folder new things go in: the selected folder, the selected file's folder, or the top.
    fn target_dir(&self) -> Option<String> {
        let sel = self.selected.as_ref()?;
        let rows = self.rows();
        match rows.iter().find(|r| &r.path == sel) {
            Some(r) if r.dir => Some(r.path.clone()),
            Some(r) => parent(&r.path),
            None => None,
        }
        .or_else(|| self.root.clone())
    }

    pub fn rows(&self) -> Vec<Row> {
        self.root.as_deref().map(|r| rows_for_tree(r, &self.children, &self.expanded, self.show_hidden)).unwrap_or_default()
    }

    fn start_prompt(&mut self, kind: Prompt, window: &mut Window, cx: &mut Context<Self>) {
        let initial = match &kind {
            Prompt::Rename(p) => p.rsplit('/').next().unwrap_or("").to_string(),
            _ => String::new(),
        };
        let input = cx.new(|cx| InputState::new(window, cx).default_value(initial));
        let sub = cx.subscribe_in(&input, window, |this, _, ev: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { .. } = ev {
                this.finish_prompt(true, window, cx);
            }
        });
        input.update(cx, |i, cx| i.focus(window, cx));
        self.prompt = Some((kind, input, sub));
        cx.notify();
    }

    pub fn cancel_prompt(&mut self, cx: &mut Context<Self>) -> bool {
        let had = self.prompt.take().is_some();
        cx.notify();
        had
    }

    fn finish_prompt(&mut self, apply: bool, _window: &mut Window, cx: &mut Context<Self>) {
        let Some((kind, input, _)) = self.prompt.take() else { return };
        let name = input.read(cx).value().trim().to_string();
        cx.notify();
        if !apply || name.is_empty() {
            return;
        }
        if name.contains('/') {
            self.message = Some(t("A name can't contain /.", "名称里不能有 /。").into());
            return;
        }
        let (Some(fs), Some(dir)) = (self.fs.clone(), self.target_dir()) else { return };
        let (op_dir, op): (String, Box<dyn FnOnce() -> Result<(), FsError> + Send>) = match kind {
            Prompt::NewFile => {
                let p = join(&dir, &name);
                (dir, Box::new(move || fs.create_file(&p)))
            }
            Prompt::NewDir => {
                let p = join(&dir, &name);
                (dir, Box::new(move || fs.create_dir(&p)))
            }
            Prompt::Rename(from) => {
                let Some(d) = parent(&from) else { return };
                let to = join(&d, &name);
                (d, Box::new(move || fs.rename(&from, &to)))
            }
        };
        self.run(op_dir, op, cx);
    }

    /// Runs a change in the background, then reloads the folder it touched.
    fn run(&mut self, dir: String, op: Box<dyn FnOnce() -> Result<(), FsError> + Send>, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async move { op() }).await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        this.message = None;
                        if this.root.as_deref() != Some(dir.as_str()) {
                            this.expanded.insert(dir.clone());
                        }
                        this.load(dir, cx);
                    }
                    Err(e) => this.message = Some(fs_error_text(&e)),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(path), Some(fs)) = (self.selected.clone(), self.fs.clone()) else { return };
        // From the listing, not the rows: the folder may be hidden or collapsed now.
        let is_dir = lookup(&self.children, &path).is_none_or(|e| e.kind == Kind::Dir);
        let detail = if is_dir {
            let count = self.children.get(&path).map(|c| format!(" ({} {})", c.len(), t("items", "项")));
            format!(
                "{path}\n{}{}",
                t("The folder and everything in it will be deleted.", "文件夹及其中的所有内容都会被删除。"),
                count.unwrap_or_default()
            )
        } else {
            path.clone()
        };
        let answer = window.prompt(
            PromptLevel::Warning,
            t("Delete?", "删除?"),
            Some(&detail),
            &[PromptButton::Ok(t("Delete", "删除").into()), PromptButton::Cancel(t("Cancel", "取消").into())],
            cx,
        );
        cx.spawn(async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let _ = this.update(cx, |this, cx| {
                let Some(dir) = parent(&path) else { return };
                let p = path.clone();
                this.selected = None;
                this.expanded.remove(&path);
                this.run(dir, Box::new(move || fs.remove(&p)), cx);
            });
        })
        .detach();
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> Div {
        let writable = self.read_only.is_none() && self.fs.is_some();
        let has_sel = self.selected.is_some();
        let why = self.read_only.clone().map(|_| t("Read-only here", "这里只读").to_string());
        let tool = |id: &'static str, icon: IconName, tip: &'static str, enabled: bool| {
            Button::new(id).ghost().size(ButtonSize::Xs).icon(icon).tooltip(tip).disabled(!enabled)
        };
        div()
            .flex()
            .flex_wrap()
            .gap_0p5()
            .child(tool("files-up", IconName::ArrowUp, t("Up", "上一级"), self.root.as_deref().is_some_and(|r| r != "/")).on_click(cx.listener(|this, _, _, cx| {
                if let Some(p) = this.root.as_deref().and_then(parent) {
                    this.go(Some(p), cx);
                }
            })))
            .child(tool("files-refresh", IconName::RefreshCw, t("Refresh", "刷新"), self.machine.is_some()).on_click(cx.listener(|this, _, _, cx| this.refresh(cx))))
            .child(
                tool("files-hidden", if self.show_hidden { IconName::Eye } else { IconName::EyeOff }, t("Show hidden files", "显示隐藏文件"), true)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_hidden = !this.show_hidden;
                        cx.notify();
                    })),
            )
            .child(
                tool("files-new-file", IconName::FilePlus, why.clone().map_or(t("New file", "新建文件"), |_| t("Read-only here", "这里只读")), writable)
                    .on_click(cx.listener(|this, _, window, cx| this.start_prompt(Prompt::NewFile, window, cx))),
            )
            .child(
                tool("files-new-dir", IconName::FolderPlus, why.clone().map_or(t("New folder", "新建文件夹"), |_| t("Read-only here", "这里只读")), writable)
                    .on_click(cx.listener(|this, _, window, cx| this.start_prompt(Prompt::NewDir, window, cx))),
            )
            .child(tool("files-rename", IconName::Pencil, t("Rename", "改名"), writable && has_sel).on_click(cx.listener(|this, _, window, cx| {
                if let Some(p) = this.selected.clone() {
                    this.start_prompt(Prompt::Rename(p), window, cx);
                }
            })))
            .child(tool("files-delete", IconName::Trash, t("Delete", "删除"), writable && has_sel).on_click(cx.listener(|this, _, window, cx| this.delete(window, cx))))
            .child(tool("files-copy", IconName::Copy, t("Copy path", "复制路径"), has_sel || self.root.is_some()).on_click(cx.listener(|this, _, _, cx| {
                if let Some(p) = this.selected.clone().or_else(|| this.root.clone()) {
                    cx.write_to_clipboard(ClipboardItem::new_string(p));
                }
            })))
            .child(tool("files-cd", IconName::SquareTerminal, t("cd the terminal here", "让终端 cd 到这里"), self.root.is_some()).on_click(cx.listener(|this, _, _, cx| {
                if let Some(d) = this.target_dir() {
                    cx.emit(PanelEvent::CdHere { path: d });
                }
            })))
    }
}

impl Render for FilePanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.sync_path_input)
            && let Some(root) = self.root.clone()
        {
            self.path_input.update(cx, |i, cx| i.set_value(root, window, cx));
        }
        let rows = self.rows();
        let selected = self.selected.clone();
        let header = div()
            .flex()
            .items_center()
            .gap_1()
            .child(
                Button::new("files-machine")
                    .ghost()
                    .size(ButtonSize::Xs)
                    .label(if self.machine_name.is_empty() { t("Files", "文件").to_string() } else { format!("{} ▾", self.machine_name) })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.picker_open = !this.picker_open;
                        cx.notify();
                    })),
            )
            .child(div().flex_1())
            .child(
                Button::new("files-follow")
                    .ghost()
                    .size(ButtonSize::Xs)
                    .icon(if self.pinned { IconName::Pin } else { IconName::Crosshair })
                    .tooltip(if self.pinned { t("Pinned — click to follow the terminal", "已固定 — 点击改为跟随终端") } else { t("Following the terminal", "跟随终端") })
                    .on_click(cx.listener(|this, _, _, cx| {
                        if this.pinned {
                            this.pinned = false;
                            cx.emit(PanelEvent::Follow);
                        }
                        cx.notify();
                    })),
            );
        let mut panel = div().size_full().flex().flex_col().gap_1().p_1().text_sm().text_color(rgb(0xd6d6d6)).child(header);
        if self.picker_open {
            let mut list = div().flex().flex_col().border_1().border_color(rgb(0x3a3a3a)).rounded_md();
            for (key, name) in &self.machines {
                let k = key.clone();
                list = list.child(
                    div()
                        .id(SharedString::from(format!("pick-{key}")))
                        .px_2()
                        .py_0p5()
                        .cursor_pointer()
                        .hover(|s| s.bg(rgb(0x2f2f2f)))
                        .child(name.clone())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.picker_open = false;
                            this.pinned = true;
                            cx.emit(PanelEvent::PickMachine(k.clone()));
                            cx.notify();
                        })),
                );
            }
            panel = panel.child(list);
        }
        panel = panel.child(Input::new(&self.path_input)).child(self.render_toolbar(cx));
        if let Some((kind, input, _)) = &self.prompt {
            let label = match kind {
                Prompt::NewFile => t("New file name", "新文件名"),
                Prompt::NewDir => t("New folder name", "新文件夹名"),
                Prompt::Rename(_) => t("New name", "新名称"),
            };
            panel = panel.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(div().text_color(rgb(0x9a9a9a)).child(label))
                    .child(Input::new(input))
                    .child(
                        div()
                            .flex()
                            .gap_1()
                            .child(Button::new("prompt-cancel").size(ButtonSize::Xs).label(t("Cancel", "取消")).on_click(cx.listener(|this, _, _, cx| {
                                this.cancel_prompt(cx);
                            })))
                            .child(Button::new("prompt-ok").primary().size(ButtonSize::Xs).label(t("OK", "确定")).on_click(cx.listener(|this, _, window, cx| this.finish_prompt(true, window, cx)))),
                    ),
            );
        }
        if let Some(m) = &self.message {
            panel = panel.child(div().text_color(rgb(0xe0a060)).child(m.clone()));
        }
        if let Some(why) = &self.read_only {
            let _ = why;
            panel = panel.child(div().text_color(rgb(0x9a9a9a)).child(t("Read-only: reached through Rex.", "只读:经 Rex 访问。")));
        }
        if let Some(u) = &self.unavailable {
            return panel.child(div().text_color(rgb(0x9a9a9a)).child(u.clone()));
        }
        if self.root.as_ref().is_some_and(|r| self.capped.contains(r)) {
            panel = panel.child(div().text_color(rgb(0x9a9a9a)).child(format!("{} {ROW_CAP}", t("Showing the first", "只显示前"))));
        }
        let count = rows.len();
        panel.child(
            uniform_list(
                "file-rows",
                count,
                cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                    let rows = this.rows();
                    range
                        .filter_map(|i| rows.get(i).cloned())
                        .map(|row| {
                            let sel = this.selected.as_deref() == Some(row.path.as_str());
                            let icon = if row.dir {
                                if row.expanded { "▾ " } else { "▸ " }
                            } else {
                                "  "
                            };
                            let r2 = row.clone();
                            div()
                                .id(SharedString::from(format!("row-{}", row.path)))
                                .pl(px(6. + 12. * row.depth as f32))
                                .pr_1()
                                .h(px(22.))
                                .flex()
                                .items_center()
                                .cursor_pointer()
                                .when(sel, |d| d.bg(rgb(0x2d4a7a)))
                                .hover(|s| s.bg(rgb(0x2f2f2f)))
                                .child(div().text_color(rgb(0x8a8a8a)).child(icon))
                                .child(div().overflow_hidden().text_ellipsis().whitespace_nowrap().child(row.name.clone()))
                                .on_click(cx.listener(move |this, _, _, cx| this.click(&r2, cx)))
                        })
                        .collect::<Vec<_>>()
                }),
            )
            .flex_1()
            .min_h_0(),
        )
        .when(selected.is_none() && count == 0 && self.root.is_some(), |d| d.child(div().text_color(rgb(0x8a8a8a)).child(t("Empty folder", "空文件夹"))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(name: &str, kind: Kind) -> FileEntry {
        FileEntry { name: name.into(), kind, size: None, modified: None, mode: None }
    }

    #[test]
    fn sort_entries_rules() {
        let list = vec![e("b.txt", Kind::File), e("Alpha", Kind::Dir), e(".env", Kind::File), e("a.txt", Kind::File), e("zlink", Kind::Symlink { to_dir: true }), e("beta", Kind::Dir)];
        let names = |v: Vec<FileEntry>| v.into_iter().map(|e| e.name).collect::<Vec<_>>();
        assert_eq!(names(sort_entries(&list, false)), ["Alpha", "beta", "zlink", "a.txt", "b.txt"]);
        assert_eq!(names(sort_entries(&list, true)), ["Alpha", "beta", "zlink", ".env", "a.txt", "b.txt"]);
    }

    #[test]
    fn lookup_finds_hidden_and_collapsed_entries() {
        let mut children = HashMap::new();
        children.insert("/r".to_string(), vec![e(".config", Kind::Dir), e("a", Kind::File)]);
        children.insert("/r/.config".to_string(), vec![e("x", Kind::File), e("y", Kind::File)]);
        assert_eq!(lookup(&children, "/r/.config").map(|e| e.kind), Some(Kind::Dir));
        assert_eq!(lookup(&children, "/r/a").map(|e| e.kind), Some(Kind::File));
        assert_eq!(lookup(&children, "/r/zzz"), None);
        assert_eq!(children.get("/r/.config").map(Vec::len), Some(2));
    }

    #[test]
    fn rows_flatten_expanded_folders() {
        let mut children = HashMap::new();
        children.insert("/r".to_string(), vec![e("src", Kind::Dir), e("README.md", Kind::File), e("docs", Kind::Dir)]);
        children.insert("/r/src".to_string(), vec![e("main.rs", Kind::File), e("lib", Kind::Dir)]);
        children.insert("/r/src/lib".to_string(), vec![e("x.rs", Kind::File)]);
        let expanded: HashSet<String> = ["/r/src".to_string(), "/r/src/lib".to_string()].into();
        let rows = rows_for_tree("/r", &children, &expanded, false);
        let flat: Vec<(String, usize, bool)> = rows.iter().map(|r| (r.path.clone(), r.depth, r.expanded)).collect();
        assert_eq!(
            flat,
            vec![
                ("/r/docs".into(), 0, false),
                ("/r/src".into(), 0, true),
                ("/r/src/lib".into(), 1, true),
                ("/r/src/lib/x.rs".into(), 2, false),
                ("/r/src/main.rs".into(), 1, false),
                ("/r/README.md".into(), 0, false),
            ]
        );
        // An expanded folder not loaded yet shows nothing under it.
        let only_root: HashSet<String> = ["/r/docs".to_string()].into();
        assert_eq!(rows_for_tree("/r", &children, &only_root, false).len(), 3);
        let _ = join("/", "x");
    }
}
