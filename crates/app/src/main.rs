//! rexa-desktop: a window showing one Rex terminal block (milestone 0).
//!
//!     rexa-desktop [-S ENDPOINT] [SESSION]

mod panes;

use std::sync::Arc;

use gpui_kit::*;
use rex_proto::control::Control;
use rex_proto::transport::Endpoint;
use std::time::Duration;

use rexterm::rex::{Attached, BlockRef, RexEvent, RexSource, attach, is_transient, recover};
use rexterm::terminal::{Terminal, TerminalConfig, TerminalState, default_key_bindings};
use serde_json::{Value, json};

/// The UI language: Chinese when the locale says so, English otherwise.
fn zh() -> bool {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
        .is_some_and(|v| v.starts_with("zh"))
}

fn t(en: &'static str, zh_text: &'static str) -> &'static str {
    if zh() { zh_text } else { en }
}

const USAGE: &str = "usage: rexa-desktop [-S ENDPOINT] [SESSION]\n";

struct Args {
    endpoint: Endpoint,
    session: Option<String>,
}

fn parse_args() -> Result<Args, String> {
    let mut endpoint = None;
    let mut session = None;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "-S" | "--server" => {
                let v = it.next().ok_or("-S needs an endpoint")?;
                endpoint = Some(Endpoint::parse(&v).map_err(|e| e.to_string())?);
            }
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            _ if session.is_none() && !a.starts_with('-') => session = Some(a),
            _ => return Err(format!("unknown argument: {a}\n{USAGE}")),
        }
    }
    Ok(Args { endpoint: endpoint.unwrap_or_else(Endpoint::default_local), session })
}

/// The window's focused block: the active window's focused block in a session view.
fn focused_block(view: &Value) -> Option<String> {
    let active = view["active_window_id"].as_str()?;
    let win = view["windows"].as_array()?.iter().find(|w| w["window_id"] == active)?;
    win["focused_block_id"].as_str().map(str::to_string)
}

struct Connected {
    block: BlockRef,
    attached: Attached,
    label: String,
}

/// Connects, picks the session (by label or id; else the first; else a new one), attaches to
/// it and opens its focused block's terminal stream. Blocks; run it off the UI thread.
fn connect(endpoint: Endpoint, wanted: Option<String>) -> Result<Connected, String> {
    let ctl = Control::connect(endpoint.clone(), "rexa-desktop").map_err(|e| e.to_string())?;
    let list = ctl.call("session.list", json!({})).map_err(|e| e.to_string())?;
    let sessions = list["sessions"].as_array().cloned().unwrap_or_default();
    let found = match &wanted {
        Some(w) => sessions.iter().find(|s| {
            s["label"] == w.as_str() || s["session_id"].as_str().is_some_and(|id| id == w || id.ends_with(w.as_str()))
        }),
        None => sessions.first(),
    };
    let sid = match (found, &wanted) {
        (Some(s), _) => s["session_id"].as_str().unwrap_or_default().to_string(),
        (None, Some(w)) => return Err(format!("{} \"{w}\"", t("no session matches", "没有匹配的 session"))),
        (None, None) => {
            let created = ctl
                .call(
                    "session.create",
                    json!({"initial_windows": [{"layout": {"block": {"flavor": "com.superlogical.terminal.shell"}}}]}),
                )
                .map_err(|e| e.to_string())?;
            created["session_id"].as_str().unwrap_or_default().to_string()
        }
    };
    let att = ctl.call("session.attach", json!({"session_id": sid})).map_err(|e| e.to_string())?;
    let label = att["view"]["label"].as_str().unwrap_or_default().to_string();
    let bid = focused_block(&att["view"]).ok_or_else(|| t("the session has no terminal", "这个 session 里没有终端").to_string())?;
    let block = BlockRef { endpoint, control: ctl, session_id: sid, block_id: bid };
    let attached = attach(&block).map_err(|e| e.to_string())?;
    Ok(Connected { block, attached, label })
}

#[derive(Clone, PartialEq)]
enum Status {
    Connecting,
    Live,
    Reconnecting,
    Exited(i64),
    Closed,
    Failed(String),
}

impl Status {
    fn text(&self) -> Option<String> {
        match self {
            Status::Live => None,
            Status::Connecting => Some(t("Connecting…", "正在连接…").into()),
            Status::Reconnecting => Some(t("Reconnecting…", "正在重连…").into()),
            Status::Closed => Some(t("Block closed", "这个 block 已关闭").into()),
            Status::Exited(code) => Some(format!("{} {code}", t("Program exited with code", "程序已退出,退出码"))),
            Status::Failed(why) => Some(format!("{}{why}", t("Can't connect: ", "连不上:"))),
        }
    }
}

struct Main {
    status: Status,
    terminal: Option<Entity<TerminalState>>,
    block: Option<BlockRef>,
    /// A recovery (re-attach and reopen the stream) is running.
    recovering: bool,
    tasks: Vec<Task<()>>,
}

impl Main {
    fn new(args: Args, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let Args { endpoint, session } = args;
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = cx.background_spawn(async move { connect(endpoint, session) }).await;
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok(c) => {
                    window.set_window_title(&format!("rexa-desktop — {}", c.label));
                    this.watch_control(c.block.control.clone(), window, cx);
                    this.show(c.block, c.attached, window, cx);
                }
                Err(why) => this.set_status(Status::Failed(why), cx),
            });
        });
        Main { status: Status::Connecting, terminal: None, block: None, recovering: false, tasks: vec![task] }
    }

    fn set_status(&mut self, status: Status, cx: &mut Context<Self>) {
        self.status = status;
        cx.notify();
    }

    /// A new terminal view on a freshly attached stream.
    fn show(&mut self, block: BlockRef, attached: Attached, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(old) = self.terminal.take() {
            old.update(cx, |t, cx| t.close(cx));
        }
        let (source, events) = RexSource::new(block.clone(), attached);
        let engine = source.engine();
        let terminal = cx.new(|cx| TerminalState::new(engine, TerminalConfig::default(), window, cx));
        cx.observe(&terminal, |_, _, cx| cx.notify()).detach();
        terminal.read(cx).focus_handle().clone().focus(window, cx);
        self.terminal = Some(terminal);
        self.block = Some(block);
        self.set_status(Status::Live, cx);
        self.tasks.push(cx.spawn_in(window, async move |this, cx| {
            while let Ok(ev) = events.recv().await {
                if this.update_in(cx, |this, window, cx| this.on_rex_event(ev, window, cx)).is_err() {
                    break;
                }
            }
        }));
    }

    fn on_rex_event(&mut self, ev: RexEvent, window: &mut Window, cx: &mut Context<Self>) {
        match ev {
            RexEvent::Reattach => self.recover(window, cx),
            RexEvent::BlockClosed => self.set_status(Status::Closed, cx),
            RexEvent::Exited(code) => self.set_status(Status::Exited(code), cx),
            // M0 keeps gpui-cn's default colours.
            RexEvent::Theme(_) => {}
        }
    }

    /// Re-attaches the session and reopens the stream, retrying while the server is
    /// unreachable, until it works or the session or block is gone.
    fn recover(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(block) = self.block.clone() else { return };
        if self.recovering || self.status == Status::Closed {
            return;
        }
        self.recovering = true;
        self.set_status(Status::Reconnecting, cx);
        self.tasks.push(cx.spawn_in(window, async move |this, cx| {
            let mut delay = Duration::from_millis(500);
            loop {
                let b = block.clone();
                let result = cx.background_spawn(async move { recover(&b) }).await;
                match result {
                    Ok(attached) => {
                        let _ = this.update_in(cx, |this, window, cx| {
                            this.recovering = false;
                            this.show(block, attached, window, cx);
                        });
                        return;
                    }
                    Err(e) if is_transient(&e) => {
                        cx.background_executor().timer(delay).await;
                        delay = (delay * 2).min(Duration::from_secs(5));
                    }
                    Err(e) => {
                        let _ = this.update(cx, |this, cx| {
                            this.recovering = false;
                            this.set_status(Status::Failed(e.to_string()), cx);
                        });
                        return;
                    }
                }
            }
        }));
    }

    fn watch_control(&mut self, control: Arc<Control>, window: &mut Window, cx: &mut Context<Self>) {
        let events = control.events();
        self.tasks.push(cx.spawn_in(window, async move |this, cx| {
            while let Ok(ev) = events.recv().await {
                let alive = this.update_in(cx, |this, window, cx| match ev["type"].as_str() {
                    Some("rexa_reconnecting") if this.status == Status::Live => this.set_status(Status::Reconnecting, cx),
                    // The server still knew this client: attachments and streams carry on.
                    Some("rexa_reconnected") if ev["resumed"] == true => {
                        if this.status == Status::Reconnecting && !this.recovering {
                            this.set_status(Status::Live, cx);
                        }
                    }
                    // A new client (gone too long, or the server restarted): attach again.
                    Some("rexa_reconnected") => this.recover(window, cx),
                    Some("rexa_disconnected") => this.set_status(Status::Failed(t("disconnected", "连接已断开").into()), cx),
                    _ => {}
                });
                if alive.is_err() {
                    break;
                }
            }
        }));
    }
}

impl Render for Main {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let mut root = div().size_full().flex().flex_col().bg(rgb(0x1e1e1e));
        if let Some(term) = &self.terminal {
            root = root.child(div().flex_1().min_h_0().child(Terminal::new("rex-terminal", term)));
        } else {
            root = root.child(div().flex_1());
        }
        if let Some(text) = self.status.text() {
            root = root.child(div().px_3().py_1().bg(rgb(0x3a3a3a)).text_color(rgb(0xe0e0e0)).text_sm().child(text));
        }
        root
    }
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprint!("rexa-desktop: {e}");
            std::process::exit(2);
        }
    };
    let mut args = Some(args);
    gpui_kit::application().with_assets(gpui_kit::assets::Assets).run(move |cx| {
        gpui_kit::init(cx);
        gpui_cn::init(cx);
        cx.bind_keys(default_key_bindings());
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        let bounds = Bounds::centered(None, size(px(1000.), px(650.)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions { title: Some("rexa-desktop".into()), ..Default::default() }),
            app_id: Some("dev.rexa.Desktop".into()),
            ..Default::default()
        };
        let args = args.take().expect("run once");
        gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| Main::new(args, window, cx)))
            .expect("failed to open window");
    });
}
