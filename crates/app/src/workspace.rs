//! The window's content: a sidebar with every machine and its sessions, a tab per Rex window
//! of the shown session, that window's panes laid out as the server says, and a status line.

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui_cn::{
    Button, CommandDialog, CommandEvent, CommandItem, CommandState, Dialog, Input, Sidebar, SidebarGroup, SidebarLayout, SidebarMenuButton,
    SidebarState, Tab, Tabs, TabsEvent, TabsState,
};
use gpui_kit::base::input::{InputEvent, InputState};
use gpui_kit::base::Selectable as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rex_client::layout::{Direction, WindowLayout, window_layout};
use rex_client::model::{Effect, SessionInfo, SessionModel, Status};
use rex_client::prefix::{Command, Dir, prefix_command};
use rex_proto::control::Control;
use rexterm::rex::{BlockRef, RexEvent, RexSource, attach, is_transient, keys::to_rex_key, recover};
use rexterm::terminal::input::{KeyInput, Modifiers};
use rexterm::terminal::{Terminal, TerminalState};
use serde_json::{Value, json};

use crate::config::{self, Settings, WorkspaceState};
use crate::i18n::t;
use crate::keys::gui_command;
use crate::machines::{ConnState, MachineSpec, StartChoice, after_reconnect, start_choice, state_target, status_line};
use crate::palette::{PaletteAction, PaletteEntry, PaletteSession, palette_entries, session_name};
use crate::machines::machine_specs;
use crate::panes::{PaneEntry, PaneSet, PaneStatus, diff};
use crate::settings_view::{SettingsEvent, SettingsView, terminal_config};

const SHELL: &str = "com.superlogical.terminal.shell";

fn shell_layout() -> Value {
    json!({"block": {"flavor": SHELL}})
}

/// Connects, lists the sessions and subscribes to every session's events. Blocks.
fn connect(endpoint: rex_proto::transport::Endpoint) -> Result<(Arc<Control>, Vec<SessionInfo>), String> {
    let control = Control::connect(endpoint, "rexa-desktop").map_err(|e| e.to_string())?;
    let list = control.call("session.list", json!({})).map_err(|e| e.to_string())?;
    let sessions = list["sessions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| Some(SessionInfo { id: s["session_id"].as_str()?.to_string(), label: s["label"].as_str().unwrap_or("").to_string() }))
        .collect();
    // Every session's events (status badges for the sessions not shown too).
    let _ = control.call("session.attach_all", json!({"event_scope": "all"}));
    Ok((control, sessions))
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

/// What the window starts with (from the command line, the settings and the saved state).
pub struct Init {
    pub specs: Vec<MachineSpec>,
    /// The machine to show first and the session wanted there.
    pub start: (String, Option<String>),
    /// The session was named on the command line: not finding it is an error.
    pub strict: bool,
    pub settings: Settings,
    pub settings_broken: bool,
    /// None: don't save the workspace (no config dir, or a machine pinned with -S).
    pub config: Option<PathBuf>,
    pub save_state: bool,
    pub state: WorkspaceState,
    pub notices: Vec<String>,
}

/// One machine: its connection, its sessions and what their programs report.
struct Machine {
    spec: MachineSpec,
    control: Option<Arc<Control>>,
    model: SessionModel,
    conn: ConnState,
    tasks: Vec<Task<()>>,
}

/// A dialog over the window.
enum Modal {
    None,
    Palette,
    Rename { machine: String, session: String, input: Entity<InputState>, _sub: Subscription },
    Settings { view: Entity<SettingsView>, _sub: Subscription },
    /// The search bar over one pane.
    Search { block: String, input: Entity<InputState>, _sub: Subscription },
}

/// Where to go once a machine connects. `machine` None: the first machine that connects.
struct Start {
    machine: Option<String>,
    session: Option<String>,
    strict: bool,
}

pub struct Workspace {
    machines: Vec<Machine>,
    /// The machine whose session is shown.
    shown: Option<String>,
    start: Option<Start>,
    settings: Settings,
    settings_broken: bool,
    config: Option<PathBuf>,
    save_enabled: bool,
    layout: WindowLayout,
    active_window: Option<String>,
    focused_block: Option<String>,
    panes: PaneSet,
    tabs: Entity<TabsState>,
    sidebar: Entity<SidebarState>,
    /// A message for the whole window (connecting, failed).
    banner: Option<String>,
    /// Standing warnings (a broken settings file, skipped machines), until clicked away or the settings are saved.
    notices: Vec<String>,
    /// The machine and session saved when the app was last closed.
    saved: (Option<String>, Option<String>),
    /// What is shown was picked by the app (start, fallback), not by the user: keep `saved`.
    automatic: bool,
    prefix: bool,
    area: Rc<Cell<Bounds<Pixels>>>,
    /// The border being dragged: (before block, after block, direction) and the ratio under the pointer.
    drag: Option<(String, String, Direction, f32)>,
    /// Keys reach the workspace even when no terminal has focus.
    focus: FocusHandle,
    _subs: Vec<Subscription>,
    fetching_view: bool,
    modal: Modal,
    palette: Entity<CommandState>,
    palette_entries: Vec<PaletteEntry>,
    tasks: Vec<Task<()>>,
}

impl Workspace {
    pub fn new(init: Init, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let tabs = cx.new(|_| TabsState::new([]));
        let st = &init.state;
        let sidebar = cx.new(|cx| {
            let s = SidebarState::new(cx).with_width(px(st.sidebar_width.unwrap_or(220.).clamp(140., 600.)));
            if st.sidebar_open { s } else { s.closed() }
        });
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        let this = cx.weak_entity();
        let key_sub = cx.intercept_keystrokes(move |ev, window, cx| {
            if let Some(w) = this.upgrade() {
                w.update(cx, |w, cx| w.on_keystroke(ev, window, cx));
            }
        });
        let palette = cx.new(|cx| CommandState::new(t("Type a command or a session…", "输入命令或 session…"), window, cx));
        cx.subscribe_in(&palette, window, |this, _, ev: &CommandEvent, window, cx| {
            if let CommandEvent::Confirmed(key) = ev {
                this.palette_chosen(key.to_string(), window, cx);
            }
        })
        .detach();
        let mut me = Self {
            machines: init
                .specs
                .into_iter()
                .map(|spec| Machine { spec, control: None, model: SessionModel::default(), conn: ConnState::Connecting, tasks: Vec::new() })
                .collect(),
            shown: None,
            start: Some(Start { machine: Some(init.start.0), session: init.start.1, strict: init.strict }),
            settings: init.settings,
            settings_broken: init.settings_broken,
            config: init.config,
            save_enabled: init.save_state,
            layout: WindowLayout::default(),
            active_window: None,
            focused_block: None,
            panes: PaneSet::default(),
            tabs: tabs.clone(),
            sidebar,
            banner: None,
            notices: init.notices,
            saved: (init.state.machine.clone(), init.state.session.clone()),
            automatic: true,
            prefix: false,
            area: Rc::new(Cell::new(Bounds::default())),
            drag: None,
            focus,
            _subs: vec![key_sub],
            fetching_view: false,
            modal: Modal::None,
            palette,
            palette_entries: Vec::new(),
            tasks: Vec::new(),
        };
        cx.subscribe_in(&tabs, window, Self::on_tab_event).detach();
        let keys: Vec<String> = me.machines.iter().map(|m| m.spec.key.clone()).collect();
        for k in keys {
            me.connect_machine(k, window, cx);
        }
        me
    }

    // ---- machines ---------------------------------------------------------------------

    fn machine(&self, key: &str) -> Option<&Machine> {
        self.machines.iter().find(|m| m.spec.key == key)
    }

    fn machine_mut(&mut self, key: &str) -> Option<&mut Machine> {
        self.machines.iter_mut().find(|m| m.spec.key == key)
    }

    fn shown_machine(&self) -> Option<&Machine> {
        self.machine(self.shown.as_deref()?)
    }

    fn shown_mut(&mut self) -> Option<&mut Machine> {
        let k = self.shown.clone()?;
        self.machine_mut(&k)
    }

    fn control(&self) -> Option<Arc<Control>> {
        self.shown_machine()?.control.clone()
    }

    fn sid(&self) -> Option<String> {
        self.shown_machine()?.model.current.clone()
    }

    fn view(&self) -> Option<&Value> {
        self.shown_machine()?.model.view.as_ref()
    }

    fn send(&self, method: &str, payload: Value) {
        if let Some(c) = self.control() {
            c.send(method, payload);
        }
    }

    /// (Re)connects a machine in the background.
    fn connect_machine(&mut self, key: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(m) = self.machine_mut(&key) else { return };
        m.conn = ConnState::Connecting;
        m.tasks.clear();
        if let Some(old) = m.control.take() {
            old.close();
        }
        let endpoint = m.spec.endpoint.clone();
        let k = key.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = cx.background_spawn(async move { connect(endpoint) }).await;
            let _ = this.update_in(cx, |this, window, cx| this.machine_connected(&k, result, window, cx));
        });
        if let Some(m) = self.machine_mut(&key) {
            m.tasks.push(task);
        }
        cx.notify();
    }

    fn machine_connected(
        &mut self,
        key: &str,
        result: Result<(Arc<Control>, Vec<SessionInfo>), String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(m) = self.machine_mut(key) else { return };
        match result {
            Err(why) => {
                m.conn = ConnState::Failed(why.clone());
                // The machine to start on can't be reached: start on the first that can.
                if let Some(s) = &mut self.start
                    && s.machine.as_deref() == Some(key)
                {
                    if s.strict {
                        self.banner = Some(format!("{}{why}", t("Can't connect: ", "连不上:")));
                        self.start = None;
                    } else {
                        s.machine = None;
                        s.session = None;
                        let first = self.machines.iter().find(|m| m.conn == ConnState::Connected).map(|m| m.spec.key.clone());
                        if let Some(k) = first {
                            self.maybe_start(&k, window, cx);
                        }
                    }
                }
            }
            Ok((control, sessions)) => {
                m.conn = ConnState::Connected;
                let previous = m.model.current.take();
                m.model = SessionModel::default();
                m.model.sessions = sessions.clone();
                m.control = Some(control.clone());
                let events = control.events();
                let k = key.to_string();
                m.tasks.push(cx.spawn_in(window, async move |this, cx| {
                    while let Ok(ev) = events.recv().await {
                        if this.update_in(cx, |this, window, cx| this.on_server_event(&k, ev, window, cx)).is_err() {
                            break;
                        }
                    }
                }));
                // Status badges for every session, in the background.
                let k = key.to_string();
                m.tasks.push(cx.spawn_in(window, async move |this, cx| {
                    let records = cx.background_spawn(async move { initial_status(&control, &sessions) }).await;
                    let _ = this.update(cx, |this, cx| {
                        if let Some(m) = this.machine_mut(&k) {
                            for (sid, bid, rid, st) in records {
                                m.model.set_status(&sid, &bid, rid, st);
                            }
                        }
                        cx.notify();
                    });
                }));
                if self.shown.as_deref() == Some(key) {
                    // The shown machine connected anew (Retry after a drop): its old panes
                    // belong to the closed connection; show the same session again.
                    self.leave(window, cx);
                    let sessions = self.machine(key).map(|m| m.model.sessions.clone()).unwrap_or_default();
                    match after_reconnect(&sessions, previous.as_deref()) {
                        Some(id) => self.open_session(id, window, cx),
                        None => self.banner = Some(t("No sessions. Press Ctrl-b N for a new one.", "没有 session。按 Ctrl-b N 新建。").into()),
                    }
                } else {
                    self.maybe_start(key, window, cx);
                }
            }
        }
        cx.notify();
    }

    /// Opens the first session once the start machine (or, after it failed, any machine) is
    /// connected: the wanted one, else the first, else a new one.
    fn maybe_start(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.shown.is_some() {
            self.start = None;
            return;
        }
        let Some(start) = &self.start else { return };
        if start.machine.as_deref().is_some_and(|m| m != key) {
            return;
        }
        let Some(m) = self.machine(key) else { return };
        let planned = start.machine.as_deref() == Some(key);
        let choice = start_choice(&m.model.sessions, start.session.as_deref(), start.strict);
        self.start = None;
        match choice {
            StartChoice::Open(id) => self.show(key.to_string(), id, !planned, window, cx),
            StartChoice::Missing(w) => self.banner = Some(format!("{} \"{w}\"", t("no session matches", "没有匹配的 session"))),
            StartChoice::Create => self.new_session(key.to_string(), window, cx),
        }
    }

    // ---- sessions ---------------------------------------------------------------------

    /// Shows `sid` of machine `key`, leaving whatever was shown: the user's choice.
    fn open(&mut self, key: String, sid: String, window: &mut Window, cx: &mut Context<Self>) {
        self.show(key, sid, false, window, cx);
    }

    /// Shows a session; `automatic`: the app picked it (keep the saved one in the state).
    fn show(&mut self, key: String, sid: String, automatic: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.shown.as_deref() == Some(key.as_str()) && self.sid().as_deref() == Some(sid.as_str()) {
            return;
        }
        self.leave(window, cx);
        self.shown = Some(key);
        self.automatic = automatic;
        self.open_session(sid, window, cx);
        self.save_state(window, cx);
    }

    /// Detaches the shown session and closes its panes.
    fn leave(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let old = self.shown_mut().and_then(|m| {
            m.model.view = None;
            m.model.reset_announced();
            m.model.current.take().map(|s| (m.control.clone(), s))
        });
        if let Some((Some(c), s)) = old {
            c.send("session.detach", json!({"session_id": s}));
        }
        self.reconcile_panes(&[], window, cx);
        self.layout = WindowLayout::default();
        self.active_window = None;
        self.focused_block = None;
        self.fetching_view = false;
    }

    /// Attaches `sid` on the shown machine and shows it (the caller has left the old one).
    fn open_session(&mut self, sid: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(control) = self.control() else { return };
        let Some(m) = self.shown_mut() else { return };
        m.model.current = Some(sid.clone());
        m.model.view = None;
        m.model.reset_announced();
        let label = m.model.sessions.iter().find(|s| s.id == sid).map(|s| session_name(&s.id, &s.label)).unwrap_or_default();
        let machine = m.spec.name.clone();
        self.active_window = None;
        self.banner = None;
        window.set_window_title(&format!("rexa-desktop — {machine} / {label}"));
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
        if let Some(k) = self.shown.clone() {
            self.open(k, sid, window, cx);
        }
    }

    /// Writes which machine and session are shown, the sidebar and the window size.
    pub fn save_state(&self, window: &Window, cx: &App) {
        let (Some(dir), true) = (&self.config, self.save_enabled) else { return };
        let sb = self.sidebar.read(cx);
        let size = window.bounds().size;
        let sid = self.sid();
        let shown = self.shown.as_deref().map(|m| (m, sid.as_deref()));
        let (machine, session) = state_target(shown, (self.saved.0.as_deref(), self.saved.1.as_deref()), self.automatic);
        let state = WorkspaceState {
            machine,
            session,
            sidebar_open: sb.is_open(),
            sidebar_width: Some(f32::from(sb.width())),
            window_size: Some((f32::from(size.width), f32::from(size.height))),
        };
        let _ = config::save_state(dir, &state);
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
                // A change announced while this fetch ran may not be in it.
                if this.shown_machine().is_some_and(|m| m.model.needs_refetch()) {
                    this.refetch_view(window, cx);
                }
            });
        }));
    }

    fn apply_view(&mut self, view: Value, window: &mut Window, cx: &mut Context<Self>) {
        if view["session_id"].as_str() != self.sid().as_deref() || !self.shown_mut().is_some_and(|m| m.model.set_view(view.clone())) {
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

    /// Focuses the server's focused block, or the workspace itself when that isn't a live
    /// terminal, so shortcuts keep working.
    fn focus_terminal(&self, window: &mut Window, cx: &mut Context<Self>) {
        match self.focused_block.as_deref().and_then(|b| self.panes.terminal(b)) {
            Some(term) => term.read(cx).focus_handle().clone().focus(window, cx),
            None => self.focus.focus(window, cx),
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
            self.view().is_some_and(|v| v["windows"].as_array().into_iter().flatten().any(|w| w["window_id"] == id))
        };
        match ev {
            // Only a selection the strip still shows: removing tabs during a sync emits
            // Selected for a neighbour that the sync then replaces with the server's choice.
            TabsEvent::Selected(id)
                if Some(id.as_ref()) != self.active_window.as_deref()
                    && in_view(id)
                    && self.tabs.read(cx).selected().map(|s| s.as_ref()) == Some(id.as_ref()) =>
            {
                self.send("session.focus_window", json!({"session_id": sid, "window_id": id.as_ref()}));
            }
            TabsEvent::AddRequested => self.run(Command::NewTab, window, cx),
            TabsEvent::Renamed(id) if in_view(id) => {
                let label = self.tabs.read(cx).tabs().iter().find(|t| t.id() == id).map(|t| t.label().to_string());
                let current = self.view().and_then(|v| {
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
        Some(BlockRef { endpoint: self.shown_machine()?.spec.endpoint.clone(), control: self.control()?, session_id: self.sid()?, block_id: block_id.to_string() })
    }

    fn reconcile_panes(&mut self, wanted: &[String], window: &mut Window, cx: &mut Context<Self>) {
        let (add, remove) = diff(&self.panes.blocks(), wanted);
        if let Modal::Search { block, .. } = &self.modal
            && remove.contains(block)
        {
            self.modal = Modal::None;
        }
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
        let terminal = cx.new(|cx| TerminalState::new(engine, terminal_config(&self.settings.font), window, cx));
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
        if let Some(m) = self.shown_mut() {
            m.model.view = None;
        }
        let _ = sid;
        self.refetch_view(window, cx);
    }

    // ---- events ---------------------------------------------------------------------

    fn on_server_event(&mut self, key: &str, ev: Value, window: &mut Window, cx: &mut Context<Self>) {
        let shown = self.shown.as_deref() == Some(key);
        let Some(m) = self.machine_mut(key) else { return };
        match ev["type"].as_str() {
            Some("rexa_reconnecting") => m.conn = ConnState::Reconnecting,
            Some("rexa_reconnected") => {
                m.conn = ConnState::Connected;
                if ev["resumed"] != true {
                    // A new client: attach everything again.
                    if let Some(c) = m.control.clone() {
                        cx.background_spawn(async move {
                            let _ = c.call("session.attach_all", json!({"event_scope": "all"}));
                        })
                        .detach();
                    }
                    if shown {
                        self.recover_all(window, cx);
                    }
                }
            }
            Some("rexa_disconnected") => m.conn = ConnState::Failed(t("disconnected", "连接已断开").into()),
            _ => match m.model.apply_event(&ev) {
                Effect::RefetchView(_) if shown => self.refetch_view(window, cx),
                Effect::SessionsChanged if shown => {
                    if let Some(cur) = m.model.current.clone()
                        && !m.model.sessions.iter().any(|s| s.id == cur)
                    {
                        // The shown session was destroyed: show another of this machine, if any.
                        let next = m.model.sessions.first().map(|s| s.id.clone());
                        m.model.current = None;
                        m.model.view = None;
                        self.reconcile_panes(&[], window, cx);
                        self.layout = WindowLayout::default();
                        self.active_window = None;
                        self.focused_block = None;
                        match next {
                            Some(next) => {
                                self.open_session(next, window, cx);
                                self.save_state(window, cx);
                            }
                            None => {
                                self.banner = Some(t("No sessions. Press Ctrl-b N for a new one.", "没有 session。按 Ctrl-b N 新建。").into());
                            }
                        }
                    }
                }
                _ => {}
            },
        }
        cx.notify();
    }

    // ---- commands -------------------------------------------------------------------

    fn run(&mut self, cmd: Command, window: &mut Window, cx: &mut Context<Self>) {
        match cmd {
            Command::Palette => return self.open_palette(window, cx),
            Command::Settings => return self.open_settings(window, cx),
            _ => {}
        }
        let Some(sid) = self.sid() else {
            if cmd == Command::NewSession {
                let first = self.shown.clone().or_else(|| self.machines.iter().find(|m| m.conn == ConnState::Connected).map(|m| m.spec.key.clone()));
                if let Some(k) = first {
                    self.new_session(k, window, cx);
                }
            }
            return;
        };
        let focused = self.focused_block.clone();
        match cmd {
            Command::NewTab => self.send("session.new_window", json!({"session_id": sid, "layout": shell_layout()})),
            Command::NextTab => self.send("session.focus_next_window", json!({"session_id": sid})),
            Command::PrevTab => self.send("session.focus_previous_window", json!({"session_id": sid})),
            Command::Tab(i) => {
                let wid = self.view().and_then(|v| v["windows"].get(i as usize)?["window_id"].as_str().map(str::to_string));
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
                let ids: Vec<String> = self.shown_machine().map(|m| m.model.sessions.iter().map(|s| s.id.clone()).collect()).unwrap_or_default();
                if let Some(i) = ids.iter().position(|s| *s == sid) {
                    let n = ids.len();
                    let j = if cmd == Command::NextSession { (i + 1) % n } else { (i + n - 1) % n };
                    self.switch_session(ids[j].clone(), window, cx);
                }
            }
            Command::NewSession => {
                if let Some(k) = self.shown.clone() {
                    self.new_session(k, window, cx);
                }
            }
            // Renaming sessions comes with the M2b palette/settings work.
            Command::RenameSession => self.open_rename(window, cx),
            Command::CloseSession => {
                let label = self.session_label(&sid);
                self.confirm(
                    t("Close this session?", "关闭这个 session?"),
                    &format!("{label} — {}", t("every tab and program in it will end.", "其中所有 tab 和程序都会结束。")),
                    t("Close", "关闭"),
                    move |this, _, _| this.send("session.destroy", json!({"session_id": sid})),
                    window,
                    cx,
                );
            }
            Command::Palette | Command::Settings => {}
            Command::Search => self.open_search(window, cx),
            Command::ToggleSidebar => {
                self.sidebar.update(cx, |s, cx| s.toggle(cx));
                self.save_state(window, cx);
            }
            Command::Quit => {
                self.save_state(window, cx);
                cx.quit();
            }
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

    fn new_session(&mut self, key: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(control) = self.machine(&key).and_then(|m| m.control.clone()) else { return };
        self.tasks.push(cx.spawn_in(window, async move |this, cx| {
            let created = cx
                .background_spawn(async move { control.call("session.create", json!({"initial_windows": [{"layout": shell_layout()}]})) })
                .await;
            match created {
                Ok(c) => {
                    let Some(id) = c["session_id"].as_str().map(str::to_string) else { return };
                    let _ = this.update_in(cx, |this, window, cx| {
                        if let Some(m) = this.machine_mut(&key)
                            && !m.model.sessions.iter().any(|s| s.id == id)
                        {
                            m.model.sessions.push(SessionInfo { id: id.clone(), label: c["label"].as_str().unwrap_or("").to_string() });
                        }
                        this.open(key, id, window, cx);
                    });
                }
                Err(e) => {
                    let _ = this.update(cx, |this, cx| {
                        this.banner = Some(format!("{}{e}", t("Can't create a session: ", "无法新建 session:")));
                        cx.notify();
                    });
                }
            }
        }));
    }

    /// Ctrl-b and the key after it, and the GUI shortcuts, before key bindings and the
    /// terminal see them.
    fn on_keystroke(&mut self, ev: &KeystrokeEvent, window: &mut Window, cx: &mut Context<Self>) {
        // A dialog takes every key: typing a name must not start commands.
        if !matches!(self.modal, Modal::None) {
            self.prefix = false;
            if matches!(self.modal, Modal::Search { .. }) && ev.keystroke.key == "escape" {
                self.close_search(window, cx);
                cx.stop_propagation();
            }
            return;
        }
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

    // ---- dialogs ----------------------------------------------------------------------

    fn session_label(&self, sid: &str) -> String {
        self.shown_machine()
            .and_then(|m| m.model.sessions.iter().find(|s| s.id == sid))
            .map(|s| session_name(&s.id, &s.label))
            .unwrap_or_default()
    }

    fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut sessions = Vec::new();
        let mut connected = Vec::new();
        for m in &self.machines {
            if m.conn == ConnState::Connected {
                connected.push((m.spec.key.as_str(), m.spec.name.as_str()));
            }
            let shown = self.shown.as_deref() == Some(m.spec.key.as_str());
            for s in &m.model.sessions {
                sessions.push(PaletteSession {
                    machine_key: &m.spec.key,
                    machine_name: &m.spec.name,
                    id: &s.id,
                    label: &s.label,
                    current: shown && m.model.current.as_deref() == Some(s.id.as_str()),
                });
            }
        }
        let entries = palette_entries(&sessions, &connected, self.sid().is_some());
        let items: Vec<CommandItem> = entries
            .iter()
            .map(|e| {
                let (label, hint) = (SharedString::from(e.label.clone()), e.hint.clone());
                CommandItem::new(e.key.clone(), e.label.clone()).render(move |_, _, _| {
                    div()
                        .flex()
                        .w_full()
                        .justify_between()
                        .child(label.clone())
                        .children(hint.clone().map(|h| div().text_color(rgb(0x8a8a8a)).child(h)))
                })
            })
            .collect();
        self.palette_entries = entries;
        self.palette.update(cx, |p, cx| p.set_entries(items, cx));
        self.modal = Modal::Palette;
        let _ = window;
        cx.notify();
    }

    fn palette_chosen(&mut self, key: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(action) = self.palette_entries.iter().find(|e| e.key == key).map(|e| e.action.clone()) else { return };
        self.modal = Modal::None;
        // After the palette has closed and handed focus back.
        self.tasks.push(cx.spawn_in(window, async move |this, cx| {
            let _ = this.update_in(cx, |this, window, cx| match action {
                PaletteAction::Run(cmd) => this.run(cmd, window, cx),
                PaletteAction::Open { machine, session } => this.open(machine, session, window, cx),
                PaletteAction::NewSession(machine) => this.new_session(machine, window, cx),
            });
        }));
        cx.notify();
    }

    fn open_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(machine), Some(session)) = (self.shown.clone(), self.sid()) else { return };
        let current = self.shown_machine().and_then(|m| m.model.sessions.iter().find(|s| s.id == session)).map(|s| s.label.clone()).unwrap_or_default();
        let input = cx.new(|cx| InputState::new(window, cx).default_value(current));
        let sub = cx.subscribe_in(&input, window, |this, _, ev: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { .. } = ev {
                this.finish_rename(true, window, cx);
            }
        });
        input.update(cx, |i, cx| i.focus(window, cx));
        self.modal = Modal::Rename { machine, session, input, _sub: sub };
        cx.notify();
    }

    fn finish_rename(&mut self, apply: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Modal::Rename { machine, session, input, .. } = std::mem::replace(&mut self.modal, Modal::None) else { return };
        let label = input.read(cx).value().trim().to_string();
        if apply
            && !label.is_empty()
            && let Some(c) = self.machine(&machine).and_then(|m| m.control.clone())
        {
            c.send("session.set_label", json!({"session_id": session, "label": label}));
        }
        self.focus_terminal(window, cx);
        cx.notify();
    }

    fn open_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(block) = self.focused_block.clone() else { return };
        let Some(term) = self.panes.terminal(&block).cloned() else { return };
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(t("Search", "搜索")));
        let sub = cx.subscribe_in(&input, window, move |_, input, ev: &InputEvent, _, cx| match ev {
            InputEvent::Change => {
                let text = input.read(cx).value().to_string();
                term.update(cx, |t, cx| t.search((!text.is_empty()).then_some(text), cx));
            }
            // Enter: older matches (up the scrollback); Shift-Enter: newer.
            InputEvent::PressEnter { shift, .. } => term.update(cx, |t, cx| t.search_step(!shift, cx)),
            InputEvent::Blur | InputEvent::Focus => {}
        });
        input.update(cx, |i, cx| i.focus(window, cx));
        if let Modal::Search { .. } = self.modal {
            self.close_search(window, cx);
        }
        self.modal = Modal::Search { block, input, _sub: sub };
        cx.notify();
    }

    fn close_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Modal::Search { block, .. } = std::mem::replace(&mut self.modal, Modal::None) else { return };
        if let Some(term) = self.panes.terminal(&block) {
            term.update(cx, |t, cx| t.search(None, cx));
        }
        self.focus_terminal(window, cx);
        cx.notify();
    }

    /// The search bar for the pane it belongs to.
    fn search_bar(&self, block: &str, cx: &mut Context<Self>) -> Option<Div> {
        let Modal::Search { block: b, input, .. } = &self.modal else { return None };
        if b != block {
            return None;
        }
        let info = self.panes.terminal(block).and_then(|t| t.read(cx).search_info().cloned());
        let count = match &info {
            Some(s) if s.total > 0 => format!("{}/{}", s.selected.map_or(0, |i| i + 1), s.total),
            Some(_) => t("no matches", "没有匹配").to_string(),
            None => String::new(),
        };
        let step = |id: &'static str, label: &'static str, older: bool, cx: &mut Context<Self>| {
            let block = block.to_string();
            Button::new(id).ghost().label(label).on_click(cx.listener(move |this, _, _, cx| {
                if let Some(term) = this.panes.terminal(&block).cloned() {
                    term.update(cx, |t, cx| t.search_step(older, cx));
                }
            }))
        };
        Some(
            div()
                .absolute()
                .top(px(4.))
                .right(px(12.))
                .w(px(380.))
                .flex()
                .items_center()
                .gap_1()
                .p_1()
                .bg(rgb(0x2b2b2b))
                .border_1()
                .border_color(rgb(0x4a4a4a))
                .rounded_md()
                .occlude()
                .child(div().flex_1().child(Input::new(input)))
                .child(div().w(px(80.)).text_sm().text_color(rgb(0xb0b0b0)).child(count))
                .child(step("search-older", "↑", true, cx))
                .child(step("search-newer", "↓", false, cx))
                .child(Button::new("search-close").ghost().label("✕").on_click(cx.listener(|this, _, window, cx| this.close_search(window, cx)))),
        )
    }

    fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let pinned = self.machines.first().is_some_and(|m| m.spec.key == "cli");
        let settings = self.settings.clone();
        let view = cx.new(|cx| SettingsView::new(&settings, pinned, window, cx));
        let sub = cx.subscribe_in(&view, window, |this, view, ev: &SettingsEvent, window, cx| match ev {
            SettingsEvent::Cancel => this.close_settings(window, cx),
            SettingsEvent::Save(s) => {
                if let Err(e) = this.apply_settings(s.clone(), window, cx) {
                    view.update(cx, |v, cx| v.set_error(e, cx));
                } else {
                    this.close_settings(window, cx);
                }
            }
        });
        self.modal = Modal::Settings { view, _sub: sub };
        cx.notify();
    }

    fn close_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.modal, Modal::Settings { .. }) {
            self.modal = Modal::None;
            self.focus_terminal(window, cx);
            cx.notify();
        }
    }

    /// Saves the settings and applies them now: language, terminal font, machines.
    fn apply_settings(&mut self, s: Settings, window: &mut Window, cx: &mut Context<Self>) -> Result<(), String> {
        if let Some(dir) = &self.config {
            config::save_settings(dir, &s, self.settings_broken).map_err(|e| format!("{}{e}", t("Can't save the settings: ", "无法保存设置:")))?;
        }
        self.settings_broken = false;
        self.notices.clear();
        crate::i18n::set_language(s.language);
        let font_changed = s.font != self.settings.font;
        self.settings = s;
        if font_changed {
            let appearance = terminal_config(&self.settings.font).appearance().clone();
            for b in self.panes.blocks() {
                if let Some(term) = self.panes.terminal(&b) {
                    term.update(cx, |t, cx| t.set_appearance(appearance.clone(), cx));
                }
            }
        }
        let pinned = self.machines.first().is_some_and(|m| m.spec.key == "cli");
        if !pinned && let Some(local) = self.machines.iter().find(|m| m.spec.local).map(|m| m.spec.endpoint.clone()) {
            let (specs, _) = machine_specs(None, &self.settings, local);
            self.set_machines(specs, window, cx);
        }
        cx.refresh_windows();
        Ok(())
    }

    /// Follows a new machine list: connects the new ones, drops the removed ones (showing
    /// another session when the shown one goes), reconnects one whose address changed.
    fn set_machines(&mut self, specs: Vec<MachineSpec>, window: &mut Window, cx: &mut Context<Self>) {
        let same = |a: &MachineSpec, b: &MachineSpec| format!("{:?}", a.endpoint) == format!("{:?}", b.endpoint);
        let gone_shown = self.shown.as_ref().is_some_and(|k| !specs.iter().any(|s| &s.key == k && self.machine(k).is_some_and(|m| same(&m.spec, s))));
        if gone_shown {
            self.leave(window, cx);
            self.shown = None;
            self.tabs.update(cx, |tabs, cx| {
                for id in tabs.tabs().iter().map(|t| t.id().to_string()).collect::<Vec<_>>() {
                    tabs.remove(id, cx);
                }
            });
        }
        let mut old: Vec<Machine> = std::mem::take(&mut self.machines);
        let mut reconnect = Vec::new();
        for spec in specs {
            match old.iter().position(|m| m.spec.key == spec.key) {
                Some(i) if same(&old[i].spec, &spec) => {
                    let mut m = old.remove(i);
                    m.spec.name = spec.name;
                    self.machines.push(m);
                }
                _ => {
                    reconnect.push(spec.key.clone());
                    self.machines.push(Machine { spec, control: None, model: SessionModel::default(), conn: ConnState::Connecting, tasks: Vec::new() });
                }
            }
        }
        for m in old {
            if let Some(c) = m.control {
                c.close();
            }
        }
        for k in reconnect {
            self.connect_machine(k, window, cx);
        }
        if self.shown.is_none() {
            self.start = Some(Start { machine: None, session: None, strict: false });
            if let Some(k) = self.machines.iter().find(|m| m.conn == ConnState::Connected).map(|m| m.spec.key.clone()) {
                self.maybe_start(&k, window, cx);
            }
            self.save_state(window, cx);
        }
    }

    fn render_modal(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let this = cx.weak_entity();
        match &self.modal {
            Modal::None | Modal::Search { .. } => None,
            Modal::Palette => Some(
                CommandDialog::new("palette", &self.palette)
                    .open(true)
                    .empty(t("Nothing matches.", "没有匹配项。"))
                    .on_open_change(move |open, window, cx| {
                        if !open {
                            let _ = this.update(cx, |this, cx| {
                                if matches!(this.modal, Modal::Palette) {
                                    this.modal = Modal::None;
                                    this.focus_terminal(window, cx);
                                    cx.notify();
                                }
                            });
                        }
                    })
                    .into_any_element(),
            ),
            Modal::Settings { view, .. } => Some(
                Dialog::new("settings")
                    .open(true)
                    .title(t("Settings", "设置"))
                    .w(px(640.))
                    .track_focus(&view.focus_handle(cx))
                    .on_open_change(move |open, window, cx| {
                        if !open {
                            let _ = this.update(cx, |this, cx| this.close_settings(window, cx));
                        }
                    })
                    .child(view.clone())
                    .into_any_element(),
            ),
            Modal::Rename { input, .. } => Some(
                Dialog::new("rename-session")
                    .open(true)
                    .title(t("Rename session", "重命名 session"))
                    .track_focus(&input.read(cx).focus_handle(cx))
                    .on_open_change(move |open, window, cx| {
                        if !open {
                            let _ = this.update(cx, |this, cx| this.finish_rename(false, window, cx));
                        }
                    })
                    .footer(
                        div()
                            .flex()
                            .gap_2()
                            .child(Button::new("rename-cancel").label(t("Cancel", "取消")).on_click(cx.listener(|this, _, window, cx| this.finish_rename(false, window, cx))))
                            .child(
                                Button::new("rename-ok")
                                    .primary()
                                    .label(t("Rename", "重命名"))
                                    .on_click(cx.listener(|this, _, window, cx| this.finish_rename(true, window, cx))),
                            ),
                    )
                    .child(Input::new(input))
                    .into_any_element(),
            ),
        }
    }

    // ---- drawing ----------------------------------------------------------------------

    fn render_sidebar(&self, cx: &mut Context<Self>) -> Sidebar {
        let mut bar = Sidebar::new();
        for m in &self.machines {
            let key = m.spec.key.clone();
            let mut group = SidebarGroup::new().id(SharedString::from(format!("machine-{key}"))).label(match m.conn.text() {
                Some(s) => format!("{} · {s}", m.spec.name),
                None => m.spec.name.clone(),
            });
            if matches!(m.conn, ConnState::Failed(_)) {
                let k = key.clone();
                group = group.child(
                    SidebarMenuButton::new(SharedString::from(format!("retry-{key}")))
                        .label(t("Retry", "重试"))
                        .on_click(cx.listener(move |this, _, window, cx| this.connect_machine(k.clone(), window, cx))),
                );
            }
            let shown = self.shown.as_deref() == Some(key.as_str());
            for s in &m.model.sessions {
                let (k, id) = (key.clone(), s.id.clone());
                let mut button = SidebarMenuButton::new(SharedString::from(format!("session-{key}-{}", s.id)))
                    .label(session_name(&s.id, &s.label))
                    .selected(shown && m.model.current.as_deref() == Some(s.id.as_str()))
                    .on_click(cx.listener(move |this, _, window, cx| this.open(k.clone(), id.clone(), window, cx)));
                if let Some(b) = status_badge(m.model.session_status(&s.id)) {
                    button = button.badge(b);
                }
                group = group.child(button);
            }
            if m.conn == ConnState::Connected {
                let k = key.clone();
                group = group.child(
                    SidebarMenuButton::new(SharedString::from(format!("new-session-{key}")))
                        .label(t("+ New session", "+ 新建 session"))
                        .on_click(cx.listener(move |this, _, window, cx| this.new_session(k.clone(), window, cx))),
                );
            }
            bar = bar.child(group);
        }
        bar
    }

    fn pane_cell(&self, p: &rex_client::layout::Pane, cx: &mut Context<Self>) -> Div {
        let r = p.rect;
        let focused = self.focused_block.as_deref() == Some(p.block_id.as_str());
        let bid = p.block_id.clone();
        let cell = div()
            .absolute()
            .left(relative(r.x))
            .top(relative(r.y))
            .w(relative(r.w))
            .h(relative(r.h))
            .p(px(1.))
            .border_1()
            .border_color(if focused { rgb(0x5a8dee) } else { rgb(0x2a2a2a) })
            .when(p.floating, |d| d.bg(rgb(0x1e1e1e)).occlude())
            // Capture phase: a program that takes the mouse stops the click in the terminal.
            .capture_any_mouse_down(cx.listener(move |this, _, _, _| {
                if this.focused_block.as_deref() != Some(bid.as_str())
                    && let Some(sid) = this.sid()
                {
                    this.send("session.focus_block", json!({"session_id": sid, "block_id": bid}));
                }
            }));
        match (p.terminal, self.panes.terminal(&p.block_id)) {
            (true, Some(term)) => {
                let mut c = cell.child(Terminal::new(SharedString::from(format!("t-{}", p.block_id)), term));
                if let Some(bar) = self.search_bar(&p.block_id, cx) {
                    c = c.child(bar);
                }
                if let Some(text) = self.panes.status(&p.block_id).text() {
                    c = c.child(div().absolute().bottom_0().left_0().right_0().px_2().bg(rgb(0x3a3a3a)).text_sm().text_color(rgb(0xe0e0e0)).child(text));
                }
                c
            }
            (true, None) => cell.child(
                div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(rgb(0x909090))
                    .child(self.panes.status(&p.block_id).text().unwrap_or_default()),
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
        }
    }

    fn render_panes(&self, cx: &mut Context<Self>) -> Div {
        let area = self.area.clone();
        let mut root = div()
            .flex_1()
            .min_h_0()
            .relative()
            .child(canvas(move |bounds, _, _| area.set(bounds), |_, _, _, _| {}).absolute().size_full());
        for p in self.layout.panes.iter().filter(|p| !p.floating) {
            root = root.child(self.pane_cell(p, cx));
        }
        for s in &self.layout.separators {
            let dragging = self.drag.as_ref().filter(|(b, a, d, _)| *b == s.before_block && *a == s.after_block && *d == s.direction);
            let at = dragging.map_or(s.ratio, |(_, _, _, r)| *r);
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
            let key = (s.before_block.clone(), s.after_block.clone(), s.direction, s.ratio);
            let hit = hit.when(dragging.is_some(), |d| d.bg(rgb(0x5a8dee))).on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    this.drag = Some(key.clone());
                    cx.stop_propagation();
                    cx.notify();
                }),
            );
            root = root.child(hit);
        }
        for p in self.layout.panes.iter().filter(|p| p.floating) {
            root = root.child(self.pane_cell(p, cx));
        }
        root
    }

    /// While a border is dragged: a layer over the whole window that follows the pointer and
    /// ends the drag wherever the button is released.
    fn drag_overlay(&self, cx: &mut Context<Self>) -> Option<Div> {
        let (_, _, direction, _) = self.drag.as_ref()?;
        let cursor = if *direction == Direction::Horizontal { CursorStyle::ResizeLeftRight } else { CursorStyle::ResizeUpDown };
        Some(
            div()
                .absolute()
                .inset_0()
                .occlude()
                .cursor(cursor)
                .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
                    let b = this.area.get();
                    if b.size.width <= px(0.) || b.size.height <= px(0.) {
                        return;
                    }
                    let fx = (ev.position.x - b.origin.x) / b.size.width;
                    let fy = (ev.position.y - b.origin.y) / b.size.height;
                    let Some((before, after, dir, _)) = this.drag.clone() else { return };
                    if let Some(s) = this.layout.separators.iter().find(|s| s.before_block == before && s.after_block == after && s.direction == dir) {
                        this.drag = Some((before, after, dir, s.ratio_at(fx, fy)));
                        cx.notify();
                    }
                }))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        let Some((before, after, dir, ratio)) = this.drag.take() else { return };
                        let still = this.layout.separators.iter().any(|s| s.before_block == before && s.after_block == after && s.direction == dir);
                        if still && let Some(sid) = this.sid() {
                            let direction = if dir == Direction::Horizontal { "horizontal" } else { "vertical" };
                            this.send(
                                "session.resize_pane",
                                json!({"session_id": sid, "direction": direction, "before_block_id": before, "after_block_id": after, "ratio": ratio}),
                            );
                        }
                        cx.notify();
                    }),
                ),
        )
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let prefix = self.prefix.then(|| t("Ctrl-b … (press a key: c new tab, % split, z zoom, : palette)", "Ctrl-b …(再按一个键:c 新 tab、% 分屏、z 放大、: 命令面板)"));
        let conn = self.shown_machine().and_then(|m| m.conn.text().map(|s| format!("{}: {s}", m.spec.name)));
        let status = status_line(prefix, self.banner.as_deref(), conn.as_deref(), &self.notices);
        let main = div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0x1e1e1e))
            .child(div().flex_none().h(px(40.)).overflow_hidden().child(Tabs::new("tabs", &self.tabs).renamable(true).confirm_close(true).addable(true)))
            .child(self.render_panes(cx))
            .when_some(status, |d, s| {
                d.child(
                    div()
                        .id("status-line")
                        .px_3()
                        .py_1()
                        .bg(rgb(0x3a3a3a))
                        .text_color(rgb(0xe0e0e0))
                        .text_sm()
                        .child(s)
                        // A click puts the standing notices away.
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.notices.clear();
                            cx.notify();
                        })),
                )
            });
        div()
            .size_full()
            .relative()
            .track_focus(&self.focus)
            .child(SidebarLayout::new(&self.sidebar).sidebar(self.render_sidebar(cx)).child(main))
            .children(self.drag_overlay(cx))
            .children(self.render_modal(cx))
    }
}
