//! The window's content: a session sidebar, a tab per Rex window, the active window's panes
//! laid out as the server says, and a status line.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui_cn::{Sidebar, SidebarGroup, SidebarLayout, SidebarMenuButton, SidebarState, Tab, Tabs, TabsEvent, TabsState};
use gpui_kit::base::Selectable as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rex_client::layout::{Direction, WindowLayout, window_layout};
use rex_client::model::{Effect, SessionInfo, SessionModel, Status};
use rex_client::prefix::{Command, Dir, prefix_command};
use rex_proto::control::Control;
use rex_proto::transport::Endpoint;
use rexterm::rex::{BlockRef, RexEvent, RexSource, attach, is_transient, keys::to_rex_key, recover};
use rexterm::terminal::input::{KeyInput, Modifiers};
use rexterm::terminal::{Terminal, TerminalConfig, TerminalState};
use serde_json::{Value, json};

use crate::i18n::t;
use crate::keys::gui_command;
use crate::panes::{PaneEntry, PaneSet, PaneStatus, diff};

const SHELL: &str = "com.superlogical.terminal.shell";

fn shell_layout() -> Value {
    json!({"block": {"flavor": SHELL}})
}

struct Connected {
    control: Arc<Control>,
    sessions: Vec<SessionInfo>,
    current: String,
}

/// Connects and picks the session: by label or id (an id suffix of 6+ characters works), else
/// the first, else a new one. Blocks.
fn connect(endpoint: Endpoint, wanted: Option<String>) -> Result<Connected, String> {
    let control = Control::connect(endpoint, "rexa-desktop").map_err(|e| e.to_string())?;
    let list = control.call("session.list", json!({})).map_err(|e| e.to_string())?;
    let mut sessions: Vec<SessionInfo> = list["sessions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| Some(SessionInfo { id: s["session_id"].as_str()?.to_string(), label: s["label"].as_str().unwrap_or("").to_string() }))
        .collect();
    let found = match &wanted {
        Some(w) => sessions.iter().find(|s| &s.label == w || &s.id == w || (w.len() >= 6 && s.id.ends_with(w.as_str()))),
        None => sessions.first(),
    };
    let current = match (found, &wanted) {
        (Some(s), _) => s.id.clone(),
        (None, Some(w)) => return Err(format!("{} \"{w}\"", t("no session matches", "没有匹配的 session"))),
        (None, None) => {
            let created = control
                .call("session.create", json!({"initial_windows": [{"layout": shell_layout()}]}))
                .map_err(|e| e.to_string())?;
            let id = created["session_id"].as_str().unwrap_or_default().to_string();
            sessions.push(SessionInfo { id: id.clone(), label: created["label"].as_str().unwrap_or("").to_string() });
            id
        }
    };
    // Every session's events (status dots for the sessions not shown too).
    let _ = control.call("session.attach_all", json!({"event_scope": "all"}));
    Ok(Connected { control, sessions, current })
}

/// The program status records of every terminal block in every session, for the sidebar.
fn initial_status(control: &Control, sessions: &[SessionInfo]) -> Vec<(String, String, String, Status)> {
    let mut out = Vec::new();
    for s in sessions {
        let Ok(view) = control.call("session.view", json!({"session_id": s.id})) else { continue };
        for w in view["windows"].as_array().into_iter().flatten() {
            for p in window_layout(w).panes.iter().filter(|p| p.terminal) {
                let Ok(r) = control.call(
                    "com.superlogical.terminal.program_status",
                    json!({"session_id": s.id, "block_id": p.block_id, "args": {}}),
                ) else {
                    continue;
                };
                for rec in r["records"].as_array().into_iter().flatten() {
                    out.push((
                        s.id.clone(),
                        p.block_id.clone(),
                        rec["id"].as_str().unwrap_or("").to_string(),
                        Status::parse(rec["state"].as_str().unwrap_or("")),
                    ));
                }
            }
        }
    }
    out
}

fn status_badge(s: Status) -> Option<&'static str> {
    match s {
        Status::Idle => None,
        Status::Working => Some("…"),
        Status::Blocked => Some("!"),
        Status::Error => Some("×"),
        Status::Done => Some("✓"),
    }
}

pub struct Workspace {
    endpoint: Endpoint,
    control: Option<Arc<Control>>,
    model: SessionModel,
    layout: WindowLayout,
    active_window: Option<String>,
    focused_block: Option<String>,
    panes: PaneSet,
    tabs: Entity<TabsState>,
    sidebar: Entity<SidebarState>,
    /// A message for the whole window (connecting, failed).
    banner: Option<String>,
    prefix: bool,
    area: Rc<Cell<Bounds<Pixels>>>,
    /// (separator index, ratio under the pointer)
    drag: Option<(usize, f32)>,
    fetching_view: bool,
    tasks: Vec<Task<()>>,
}

impl Workspace {
    pub fn new(endpoint: Endpoint, session: Option<String>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let tabs = cx.new(|_| TabsState::new([]));
        let sidebar = cx.new(|cx| SidebarState::new(cx).with_width(px(220.)));
        let mut tasks = Vec::new();
        let ep = endpoint.clone();
        tasks.push(cx.spawn_in(window, async move |this, cx| {
            let result = cx.background_spawn(async move { connect(ep, session) }).await;
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok(c) => this.connected(c, window, cx),
                Err(why) => {
                    this.banner = Some(format!("{}{why}", t("Can't connect: ", "连不上:")));
                    cx.notify();
                }
            });
        }));
        let me = Self {
            endpoint,
            control: None,
            model: SessionModel::default(),
            layout: WindowLayout::default(),
            active_window: None,
            focused_block: None,
            panes: PaneSet::default(),
            tabs: tabs.clone(),
            sidebar,
            banner: Some(t("Connecting…", "正在连接…").into()),
            prefix: false,
            area: Rc::new(Cell::new(Bounds::default())),
            drag: None,
            fetching_view: false,
            tasks,
        };
        cx.subscribe_in(&tabs, window, Self::on_tab_event).detach();
        me
    }

    fn control(&self) -> Option<Arc<Control>> {
        self.control.clone()
    }

    fn sid(&self) -> Option<String> {
        self.model.current.clone()
    }

    fn send(&self, method: &str, payload: Value) {
        if let Some(c) = &self.control {
            c.send(method, payload);
        }
    }

    fn connected(&mut self, c: Connected, window: &mut Window, cx: &mut Context<Self>) {
        self.banner = None;
        self.model.sessions = c.sessions.clone();
        self.control = Some(c.control.clone());
        self.watch_events(c.control.clone(), window, cx);
        // Status dots for every session, in the background.
        let (control, sessions) = (c.control.clone(), c.sessions);
        self.tasks.push(cx.spawn_in(window, async move |this, cx| {
            let records = cx.background_spawn(async move { initial_status(&control, &sessions) }).await;
            let _ = this.update(cx, |this, cx| {
                for (sid, bid, rid, st) in records {
                    this.model.set_status(&sid, &bid, rid, st);
                }
                cx.notify();
            });
        }));
        self.open_session(c.current, window, cx);
    }

    /// Attaches `sid` and shows it (the caller has left the old one).
    fn open_session(&mut self, sid: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(control) = self.control() else { return };
        self.model.current = Some(sid.clone());
        self.model.view = None;
        self.active_window = None;
        let label = self.model.sessions.iter().find(|s| s.id == sid).map(|s| s.label.clone()).unwrap_or_default();
        window.set_window_title(&format!("rexa-desktop — {label}"));
        self.tasks.push(cx.spawn_in(window, async move |this, cx| {
            let c2 = control.clone();
            let s2 = sid.clone();
            let att = cx.background_spawn(async move { c2.call("session.attach", json!({"session_id": s2})) }).await;
            let _ = this.update_in(cx, |this, window, cx| match att {
                Ok(v) => this.apply_view(v["view"].clone(), window, cx),
                Err(e) => {
                    this.banner = Some(format!("{}{e}", t("Can't open the session: ", "打不开 session:")));
                    cx.notify();
                }
            });
        }));
    }

    fn switch_session(&mut self, sid: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(old) = self.sid() else { return };
        if old == sid {
            return;
        }
        self.send("session.detach", json!({"session_id": old}));
        self.reconcile_panes(&[], window, cx);
        self.layout = WindowLayout::default();
        self.open_session(sid, window, cx);
    }

    fn refetch_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(control), Some(sid)) = (self.control(), self.sid()) else { return };
        if self.fetching_view {
            return;
        }
        self.fetching_view = true;
        self.tasks.push(cx.spawn_in(window, async move |this, cx| {
            let v = cx.background_spawn(async move { control.call("session.view", json!({"session_id": sid})) }).await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.fetching_view = false;
                if let Ok(v) = v {
                    this.apply_view(v, window, cx);
                }
            });
        }));
    }

    fn apply_view(&mut self, view: Value, window: &mut Window, cx: &mut Context<Self>) {
        if view["session_id"].as_str() != self.sid().as_deref() || !self.model.set_view(view.clone()) {
            return;
        }
        let active = view["active_window_id"].as_str().map(str::to_string);
        let win = view["windows"].as_array().and_then(|ws| ws.iter().find(|w| w["window_id"].as_str() == active.as_deref())).cloned();
        self.layout = win.as_ref().map(window_layout).unwrap_or_default();
        let wanted: Vec<String> = self.layout.panes.iter().filter(|p| p.terminal).map(|p| p.block_id.clone()).collect();
        self.reconcile_panes(&wanted, window, cx);
        self.sync_tabs(&view, active.as_deref(), window, cx);
        self.active_window = active;
        let focused = win.as_ref().and_then(|w| w["focused_block_id"].as_str().map(str::to_string));
        if focused != self.focused_block {
            self.focused_block = focused;
            self.focus_terminal(window, cx);
        }
        cx.notify();
    }

    fn focus_terminal(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(term) = self.focused_block.as_deref().and_then(|b| self.panes.terminal(b)) {
            term.read(cx).focus_handle().clone().focus(window, cx);
        }
    }

    fn sync_tabs(&mut self, view: &Value, active: Option<&str>, window: &mut Window, cx: &mut Context<Self>) {
        let wanted: Vec<(String, String)> = view["windows"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
            .filter_map(|(i, w)| {
                let id = w["window_id"].as_str()?.to_string();
                let label = w["label"].as_str().filter(|l| !l.is_empty()).map(str::to_string).unwrap_or_else(|| format!("{}", i + 1));
                Some((id, label))
            })
            .collect();
        self.tabs.update(cx, |tabs, cx| {
            let have: Vec<(String, String)> = tabs.tabs().iter().map(|t| (t.id().to_string(), t.label().to_string())).collect();
            if have != wanted {
                for (id, _) in &have {
                    if !wanted.iter().any(|(w, _)| w == id) {
                        tabs.remove(id.clone(), cx);
                    }
                }
                for (i, (id, label)) in wanted.iter().enumerate() {
                    match have.iter().find(|(h, _)| h == id) {
                        Some((_, l)) if l != label => tabs.rename(id.clone(), label.clone(), cx),
                        Some(_) => {}
                        None => tabs.insert(i.min(tabs.tabs().len()), Tab::new(id.clone(), label.clone()), cx),
                    }
                }
            }
            if let Some(a) = active
                && tabs.selected().map(|s| s.as_ref()) != Some(a)
            {
                tabs.select(a.to_string(), cx);
            }
        });
        let _ = window;
    }

    fn on_tab_event(&mut self, _tabs: &Entity<TabsState>, ev: &TabsEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(sid) = self.sid() else { return };
        let in_view = |id: &str| {
            self.model.view.as_ref().is_some_and(|v| v["windows"].as_array().into_iter().flatten().any(|w| w["window_id"] == id))
        };
        match ev {
            TabsEvent::Selected(id) if Some(id.as_ref()) != self.active_window.as_deref() && in_view(id) => {
                self.send("session.focus_window", json!({"session_id": sid, "window_id": id.as_ref()}));
            }
            TabsEvent::AddRequested => self.run(Command::NewTab, window, cx),
            TabsEvent::Renamed(id) if in_view(id) => {
                let label = self.tabs.read(cx).tabs().iter().find(|t| t.id() == id).map(|t| t.label().to_string());
                let current = self.model.view.as_ref().and_then(|v| {
                    v["windows"].as_array()?.iter().find(|w| w["window_id"] == id.as_ref())?["label"].as_str().map(str::to_string)
                });
                if let Some(l) = label
                    && Some(&l) != current.as_ref()
                {
                    self.send("session.set_window_label", json!({"session_id": sid, "window_id": id.as_ref(), "label": l}));
                }
            }
            TabsEvent::CloseRequested(id) if in_view(id) => {
                let id = id.to_string();
                self.confirm(
                    t("Close this tab?", "关闭这个 tab?"),
                    t("Every program in it will end.", "里面的程序都会结束。"),
                    t("Close", "关闭"),
                    move |this, _, _| this.send("session.close_window", json!({"session_id": sid, "window_id": id})),
                    window,
                    cx,
                );
            }
            _ => {}
        }
    }

    /// Asks the user, then runs `yes`.
    fn confirm(
        &mut self,
        message: &str,
        detail: &str,
        ok: &str,
        yes: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let answer = window.prompt(
            PromptLevel::Warning,
            message,
            Some(detail),
            &[PromptButton::Ok(SharedString::from(ok.to_string())), PromptButton::Cancel(t("Cancel", "取消").into())],
            cx,
        );
        self.tasks.push(cx.spawn_in(window, async move |this, cx| {
            if answer.await == Ok(0) {
                let _ = this.update_in(cx, |this, window, cx| yes(this, window, cx));
            }
        }));
    }

    // ---- panes --------------------------------------------------------------------

    fn block_ref(&self, block_id: &str) -> Option<BlockRef> {
        Some(BlockRef { endpoint: self.endpoint.clone(), control: self.control()?, session_id: self.sid()?, block_id: block_id.to_string() })
    }

    fn reconcile_panes(&mut self, wanted: &[String], window: &mut Window, cx: &mut Context<Self>) {
        let (add, remove) = diff(&self.panes.blocks(), wanted);
        for b in remove {
            if let Some(e) = self.panes.entries.remove(&b)
                && let Some(term) = e.terminal
            {
                term.update(cx, |t, cx| t.close(cx));
            }
        }
        for b in add {
            self.panes.entries.insert(b.clone(), PaneEntry { terminal: None, status: PaneStatus::Connecting, recovering: false, tasks: Vec::new() });
            self.start_pane(b, false, window, cx);
        }
    }

    /// Attaches (or, with `recovering`, re-attaches the session and then attaches) the block's
    /// stream, retrying while that may still work, then shows it.
    fn start_pane(&mut self, block_id: String, recovering: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(block) = self.block_ref(&block_id) else { return };
        let task = cx.spawn_in(window, async move |this, cx| {
            let mut delay = Duration::from_millis(500);
            loop {
                let b = block.clone();
                let result = cx.background_spawn(async move { if recovering { recover(&b) } else { attach(&b) } }).await;
                match result {
                    Ok(attached) => {
                        let _ = this.update_in(cx, |this, window, cx| this.show_pane(block, attached, window, cx));
                        return;
                    }
                    Err(e) if is_transient(&e) => {
                        let still = this
                            .update(cx, |this, _| {
                                this.panes.entries.get_mut(&block.block_id).map(|en| en.status = PaneStatus::Reconnecting).is_some()
                            })
                            .unwrap_or(false);
                        if !still {
                            return;
                        }
                        cx.background_executor().timer(delay).await;
                        delay = (delay * 2).min(Duration::from_secs(5));
                    }
                    Err(e) => {
                        let _ = this.update(cx, |this, cx| {
                            if let Some(en) = this.panes.entries.get_mut(&block.block_id) {
                                en.status = PaneStatus::Failed(e.to_string());
                                en.recovering = false;
                            }
                            cx.notify();
                        });
                        return;
                    }
                }
            }
        });
        if let Some(e) = self.panes.entries.get_mut(&block_id) {
            e.recovering = recovering;
            e.tasks.push(task);
        }
    }

    fn show_pane(&mut self, block: BlockRef, attached: rexterm::rex::Attached, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.panes.entries.get_mut(&block.block_id) else {
            // Gone from the layout while attaching: end the stream.
            attached.conn.close();
            return;
        };
        if let Some(old) = entry.terminal.take() {
            old.update(cx, |t, cx| t.close(cx));
        }
        let (source, events) = RexSource::new(block.clone(), attached);
        let engine = source.engine();
        let terminal = cx.new(|cx| TerminalState::new(engine, TerminalConfig::default(), window, cx));
        cx.observe(&terminal, |_, _, cx| cx.notify()).detach();
        let id = block.block_id.clone();
        entry.terminal = Some(terminal);
        entry.status = PaneStatus::Live;
        entry.recovering = false;
        entry.tasks.push(cx.spawn_in(window, async move |this, cx| {
            while let Ok(ev) = events.recv().await {
                if this.update_in(cx, |this, window, cx| this.on_pane_event(&id, ev, window, cx)).is_err() {
                    break;
                }
            }
        }));
        if self.focused_block.as_deref() == Some(block.block_id.as_str()) {
            self.focus_terminal(window, cx);
        }
        cx.notify();
    }

    fn on_pane_event(&mut self, block_id: &str, ev: RexEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.panes.entries.get_mut(block_id) else { return };
        match ev {
            RexEvent::Reattach => {
                if !entry.recovering {
                    entry.status = PaneStatus::Reconnecting;
                    self.start_pane(block_id.to_string(), true, window, cx);
                }
            }
            RexEvent::BlockClosed => entry.status = PaneStatus::Closed,
            RexEvent::Exited(code) => entry.status = PaneStatus::Exited(code),
            RexEvent::Theme(_) => {}
        }
        cx.notify();
    }

    fn recover_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(sid) = self.sid() else { return };
        for b in self.panes.blocks() {
            if let Some(e) = self.panes.entries.get_mut(&b)
                && !e.recovering
            {
                e.status = PaneStatus::Reconnecting;
                self.start_pane(b, true, window, cx);
            }
        }
        // The new client is attached again by the first recover; read the view afresh.
        self.model.view = None;
        let _ = sid;
        self.refetch_view(window, cx);
    }

    // ---- events ---------------------------------------------------------------------

    fn watch_events(&mut self, control: Arc<Control>, window: &mut Window, cx: &mut Context<Self>) {
        let events = control.events();
        self.tasks.push(cx.spawn_in(window, async move |this, cx| {
            while let Ok(ev) = events.recv().await {
                if this.update_in(cx, |this, window, cx| this.on_server_event(ev, window, cx)).is_err() {
                    break;
                }
            }
        }));
    }

    fn on_server_event(&mut self, ev: Value, window: &mut Window, cx: &mut Context<Self>) {
        match ev["type"].as_str() {
            Some("rexa_reconnecting") => self.banner = Some(t("Reconnecting…", "正在重连…").into()),
            Some("rexa_reconnected") => {
                self.banner = None;
                if ev["resumed"] != true {
                    // A new client: attach everything again.
                    if let (Some(c), Some(sid)) = (self.control(), self.sid()) {
                        let _ = c.call("session.attach_all", json!({"event_scope": "all"}));
                        let _ = sid;
                    }
                    self.recover_all(window, cx);
                }
            }
            Some("rexa_disconnected") => self.banner = Some(t("Disconnected", "连接已断开").into()),
            _ => match self.model.apply_event(&ev) {
                Effect::RefetchView(_) => self.refetch_view(window, cx),
                Effect::SessionsChanged => {
                    if let Some(cur) = self.sid()
                        && !self.model.sessions.iter().any(|s| s.id == cur)
                    {
                        // The shown session was destroyed: show another, if any.
                        self.reconcile_panes(&[], window, cx);
                        self.layout = WindowLayout::default();
                        match self.model.sessions.first().map(|s| s.id.clone()) {
                            Some(next) => self.open_session(next, window, cx),
                            None => {
                                self.model.current = None;
                                self.banner = Some(t("No sessions. Press Ctrl-b N for a new one.", "没有 session。按 Ctrl-b N 新建。").into());
                            }
                        }
                    }
                }
                Effect::StatusChanged | Effect::Nothing => {}
            },
        }
        cx.notify();
    }

    // ---- commands -------------------------------------------------------------------

    fn run(&mut self, cmd: Command, window: &mut Window, cx: &mut Context<Self>) {
        let Some(sid) = self.sid() else {
            if cmd == Command::NewSession {
                self.new_session(window, cx);
            }
            return;
        };
        let focused = self.focused_block.clone();
        match cmd {
            Command::NewTab => self.send("session.new_window", json!({"session_id": sid, "layout": shell_layout()})),
            Command::NextTab => self.send("session.focus_next_window", json!({"session_id": sid})),
            Command::PrevTab => self.send("session.focus_previous_window", json!({"session_id": sid})),
            Command::Tab(i) => {
                let wid = self.model.view.as_ref().and_then(|v| v["windows"].get(i as usize)?["window_id"].as_str().map(str::to_string));
                if let Some(w) = wid {
                    self.send("session.focus_window", json!({"session_id": sid, "window_id": w}));
                }
            }
            Command::SplitRight | Command::SplitDown => {
                let direction = if cmd == Command::SplitRight { "horizontal" } else { "vertical" };
                self.send(
                    "session.new_split",
                    json!({"session_id": sid, "direction": direction, "side": "after", "focus": true, "layout": shell_layout()}),
                );
            }
            Command::Focus(d) => {
                let dir = match d {
                    Dir::Left => "left",
                    Dir::Right => "right",
                    Dir::Up => "up",
                    Dir::Down => "down",
                };
                self.send("session.focus_direction", json!({"session_id": sid, "direction": dir, "wrap": true}));
            }
            Command::FocusNext => self.send("session.focus_next_pane", json!({"session_id": sid})),
            Command::Zoom => self.send("session.zoom_block", json!({"session_id": sid})),
            Command::ClosePane => {
                if let Some(b) = focused {
                    self.confirm(
                        t("Close this pane?", "关闭这个 pane?"),
                        t("Its program will end.", "里面的程序会结束。"),
                        t("Close", "关闭"),
                        move |this, _, _| this.send("block.close", json!({"session_id": sid, "block_id": b})),
                        window,
                        cx,
                    );
                }
            }
            Command::RenameTab => {
                if let Some(w) = self.active_window.clone() {
                    self.tabs.update(cx, |tabs, cx| tabs.start_rename(w, window, cx));
                }
            }
            Command::PrevSession | Command::NextSession => {
                let ids: Vec<String> = self.model.sessions.iter().map(|s| s.id.clone()).collect();
                if let Some(i) = ids.iter().position(|s| *s == sid) {
                    let n = ids.len();
                    let j = if cmd == Command::NextSession { (i + 1) % n } else { (i + n - 1) % n };
                    self.switch_session(ids[j].clone(), window, cx);
                }
            }
            Command::NewSession => self.new_session(window, cx),
            // Renaming sessions comes with the M2b palette/settings work.
            Command::RenameSession => {}
            Command::ToggleSidebar => self.sidebar.update(cx, |s, cx| s.toggle(cx)),
            Command::Quit => cx.quit(),
            Command::SendPrefix => {
                if let Some(b) = focused {
                    let mut m = Modifiers::default();
                    m.control = true;
                    if let Some(key) = to_rex_key(&KeyInput::new("b").with_modifiers(m)) {
                        self.send("client.key", json!({"session_id": sid, "block_id": b, "key": key}));
                    }
                }
            }
        }
    }

    fn new_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(control) = self.control() else { return };
        self.tasks.push(cx.spawn_in(window, async move |this, cx| {
            let created = cx
                .background_spawn(async move { control.call("session.create", json!({"initial_windows": [{"layout": shell_layout()}]})) })
                .await;
            if let Ok(c) = created
                && let Some(id) = c["session_id"].as_str().map(str::to_string)
            {
                let _ = this.update_in(cx, |this, window, cx| {
                    if !this.model.sessions.iter().any(|s| s.id == id) {
                        this.model.sessions.push(SessionInfo { id: id.clone(), label: c["label"].as_str().unwrap_or("").to_string() });
                    }
                    if this.sid().is_some() {
                        this.switch_session(id, window, cx);
                    } else {
                        this.banner = None;
                        this.open_session(id, window, cx);
                    }
                });
            }
        }));
    }

    /// Ctrl-b and the key after it, and the GUI shortcuts, before the terminal sees them.
    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        if self.prefix {
            self.prefix = false;
            if let Some(cmd) = prefix_command(&ks.key, ks.modifiers.shift, ks.modifiers.control) {
                self.run(cmd, window, cx);
            }
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if ks.modifiers.control && !ks.modifiers.shift && !ks.modifiers.alt && !ks.modifiers.platform && ks.key == "b" {
            self.prefix = true;
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if let Some(cmd) = gui_command(ks) {
            self.run(cmd, window, cx);
            cx.stop_propagation();
        }
    }

    // ---- drawing ----------------------------------------------------------------------

    fn render_sidebar(&self, cx: &mut Context<Self>) -> Sidebar {
        let current = self.sid();
        let mut group = SidebarGroup::new().label(t("Sessions", "Session"));
        for s in &self.model.sessions {
            let id = s.id.clone();
            let label = if s.label.is_empty() { s.id.chars().rev().take(6).collect::<String>().chars().rev().collect() } else { s.label.clone() };
            let mut button = SidebarMenuButton::new(SharedString::from(format!("session-{}", s.id)))
                .label(label)
                .selected(current.as_deref() == Some(s.id.as_str()))
                .on_click(cx.listener(move |this, _, window, cx| this.switch_session(id.clone(), window, cx)));
            if let Some(b) = status_badge(self.model.session_status(&s.id)) {
                button = button.badge(b);
            }
            group = group.child(button);
        }
        Sidebar::new()
            .header(div().px_2().py_1().text_sm().child(t("This computer", "本机")))
            .child(group)
            .footer(
                SidebarMenuButton::new("new-session")
                    .label(t("+ New session", "+ 新建 session"))
                    .on_click(cx.listener(|this, _, window, cx| this.new_session(window, cx))),
            )
    }

    fn render_panes(&self, cx: &mut Context<Self>) -> Div {
        let area = self.area.clone();
        let mut root = div()
            .flex_1()
            .min_h_0()
            .relative()
            .child(canvas(move |bounds, _, _| area.set(bounds), |_, _, _, _| {}).absolute().size_full());
        for p in &self.layout.panes {
            let r = p.rect;
            let focused = self.focused_block.as_deref() == Some(p.block_id.as_str());
            let bid = p.block_id.clone();
            let mut cell = div()
                .absolute()
                .left(relative(r.x))
                .top(relative(r.y))
                .w(relative(r.w))
                .h(relative(r.h))
                .p(px(1.))
                .border_1()
                .border_color(if focused { rgb(0x5a8dee) } else { rgb(0x2a2a2a) })
                .when(p.floating, |d| d.bg(rgb(0x1e1e1e)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, _| {
                        if this.focused_block.as_deref() != Some(bid.as_str())
                            && let Some(sid) = this.sid()
                        {
                            this.send("session.focus_block", json!({"session_id": sid, "block_id": bid}));
                        }
                    }),
                );
            cell = match (p.terminal, self.panes.terminal(&p.block_id)) {
                (true, Some(term)) => {
                    let mut c = cell.child(Terminal::new(SharedString::from(format!("t-{}", p.block_id)), term));
                    if let Some(text) = self.panes.status(&p.block_id).text() {
                        c = c.child(div().absolute().bottom_0().left_0().right_0().px_2().bg(rgb(0x3a3a3a)).text_sm().text_color(rgb(0xe0e0e0)).child(text));
                    }
                    c
                }
                (true, None) => cell.child(
                    div().size_full().flex().items_center().justify_center().text_color(rgb(0x909090)).child(
                        self.panes.status(&p.block_id).text().unwrap_or_default(),
                    ),
                ),
                (false, _) => cell.child(
                    div()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_color(rgb(0x909090))
                        .child(format!("{} {}", p.label, t("(not a terminal)", "(不是终端)"))),
                ),
            };
            root = root.child(cell);
        }
        for (i, s) in self.layout.separators.iter().enumerate() {
            let at = match self.drag {
                Some((d, ratio)) if d == i => ratio,
                _ => s.ratio,
            };
            let hit = match s.direction {
                Direction::Horizontal => div()
                    .absolute()
                    .left(relative(s.rect.x + s.rect.w * at))
                    .ml(px(-3.))
                    .top(relative(s.rect.y))
                    .w(px(6.))
                    .h(relative(s.rect.h))
                    .cursor(CursorStyle::ResizeLeftRight),
                Direction::Vertical => div()
                    .absolute()
                    .top(relative(s.rect.y + s.rect.h * at))
                    .mt(px(-3.))
                    .left(relative(s.rect.x))
                    .h(px(6.))
                    .w(relative(s.rect.w))
                    .cursor(CursorStyle::ResizeUpDown),
            };
            let hit = hit
                .when(self.drag.is_some_and(|(d, _)| d == i), |d| d.bg(rgb(0x5a8dee)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        let r = this.layout.separators.get(i).map(|s| s.ratio).unwrap_or(0.5);
                        this.drag = Some((i, r));
                        cx.stop_propagation();
                        cx.notify();
                    }),
                );
            root = root.child(hit);
        }
        root.on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
            let Some((i, _)) = this.drag else { return };
            let b = this.area.get();
            if b.size.width <= px(0.) || b.size.height <= px(0.) {
                return;
            }
            let fx = (ev.position.x - b.origin.x) / b.size.width;
            let fy = (ev.position.y - b.origin.y) / b.size.height;
            if let Some(s) = this.layout.separators.get(i) {
                this.drag = Some((i, s.ratio_at(fx, fy)));
                cx.notify();
            }
        }))
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|this, _, _, cx| {
                let Some((i, ratio)) = this.drag.take() else { return };
                if let (Some(s), Some(sid)) = (this.layout.separators.get(i), this.sid()) {
                    let direction = if s.direction == Direction::Horizontal { "horizontal" } else { "vertical" };
                    this.send(
                        "session.resize_pane",
                        json!({"session_id": sid, "direction": direction, "before_block_id": s.before_block, "after_block_id": s.after_block, "ratio": ratio}),
                    );
                }
                cx.notify();
            }),
        )
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let status = if self.prefix {
            Some(t("Ctrl-b … (press a key: c new tab, % split, z zoom, ? help)", "Ctrl-b …(再按一个键:c 新 tab、% 分屏、z 放大)").to_string())
        } else {
            self.banner.clone()
        };
        let main = div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0x1e1e1e))
            .child(div().flex_none().h(px(40.)).overflow_hidden().child(Tabs::new("tabs", &self.tabs).renamable(true).confirm_close(true).addable(true)))
            .child(self.render_panes(cx))
            .when_some(status, |d, s| d.child(div().px_3().py_1().bg(rgb(0x3a3a3a)).text_color(rgb(0xe0e0e0)).text_sm().child(s)));
        div()
            .size_full()
            .capture_key_down(cx.listener(Self::on_key))
            .child(SidebarLayout::new(&self.sidebar).sidebar(self.render_sidebar(cx)).child(main))
    }
}
