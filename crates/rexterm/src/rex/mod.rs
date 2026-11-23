//! A Rex terminal block as a gpui-cn byte source.

mod attach;
pub mod keys;

pub use attach::{AttachError, Attached, BlockRef, attach, is_transient, recover};

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gpui_cn::terminal::{
    ByteHandle, ByteSink, ByteSource, Engine, EngineOptions, ExitStatus, InputRoute, TerminalInput, Viewport,
};
use gpui_cn::terminal::input::{MouseAction, MouseButton, MouseInput};
use rex_proto::data::{DataConn, DataError, DataItem, TERMINAL_V1};
use rex_proto::scan::{ScanItem, Scanner};
use serde_json::json;

/// What the source tells the UI that the terminal view can't show by itself.
#[derive(Debug, Clone, PartialEq)]
pub enum RexEvent {
    /// The stream can't be resumed: attach again and build a new engine.
    Reattach,
    /// The block closed.
    BlockClosed,
    /// The program exited with this code (the block stays open).
    Exited(i64),
    /// New default colors, as Rex's `theme` message has them.
    Theme(serde_json::Value),
}

pub struct RexSource {
    block: BlockRef,
    attached: Attached,
    events: async_channel::Sender<RexEvent>,
}

impl RexSource {
    pub fn new(block: BlockRef, attached: Attached) -> (RexSource, async_channel::Receiver<RexEvent>) {
        let (events, rx) = async_channel::unbounded();
        (RexSource { block, attached, events }, rx)
    }

    /// An engine that starts from the stream's snapshot and takes its size from the stream.
    pub fn engine(self) -> Engine<RexSource> {
        let snapshot = self.attached.snapshot.clone();
        Engine::new(self, EngineOptions::default().with_snapshot(snapshot).with_source_sized(true))
    }
}

impl ByteSource for RexSource {
    fn start(self: Box<Self>, sink: ByteSink, _viewport: Viewport) -> io::Result<Box<dyn ByteHandle>> {
        let RexSource { block, attached, events } = *self;
        let shared = Arc::new(Shared {
            closing: AtomicBool::new(false),
            closed_code: Mutex::new(None),
            conn_closer: Mutex::new(None),
            resize: Mutex::new((0, None)),
            cell: Mutex::new((8, 16)),
        });
        let reader = Reader { block: block.clone(), sink, events, shared: shared.clone() };
        std::thread::Builder::new().name("rex-terminal".into()).spawn(move || reader.run(attached))?;
        Ok(Box::new(RexHandle { block, shared }))
    }
}

struct Shared {
    closing: AtomicBool,
    /// Set when the stream ended for good (the block closed).
    closed_code: Mutex<Option<u16>>,
    /// Shuts the current data connection down (to stop the reader).
    conn_closer: Mutex<Option<Box<dyn Fn() + Send>>>,
    /// (generation, latest requested size) for the debounced resize request.
    resize: Mutex<(u64, Option<(u16, u16)>)>,
    /// The cell size in device pixels, for pointer positions.
    cell: Mutex<(u32, u32)>,
}

/// Delay before a UI size is sent to Rex, so a drag doesn't flood the server.
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(50);

struct Reader {
    block: BlockRef,
    sink: ByteSink,
    events: async_channel::Sender<RexEvent>,
    shared: Arc<Shared>,
}

impl Reader {
    fn run(self, attached: Attached) {
        let Attached { rest, mut conn, .. } = attached;
        let mut scanner = Scanner::new(conn.nonce);
        let mut items = Vec::new();
        scanner.feed(&rest, &mut items);
        self.deliver(&mut items);
        loop {
            if self.shared.closing.load(Ordering::Acquire) {
                conn.close();
                return;
            }
            self.set_closer(&conn);
            let item = conn.next().unwrap_or(DataItem::Closed(1006));
            match item {
                DataItem::Bytes(b) => {
                    if !self.sink.wait_for_room() {
                        return;
                    }
                    scanner.feed(&b, &mut items);
                    self.deliver(&mut items);
                }
                DataItem::Closed(code) => {
                    if self.shared.closing.load(Ordering::Acquire) {
                        return;
                    }
                    match code {
                        1000 | 4000 => return self.ended(code),
                        1013 => return self.reattach(),
                        _ => match self.resume(conn.token, conn.received, conn.resume_ms) {
                            Some(c) => conn = c,
                            None => return self.reattach(),
                        },
                    }
                }
            }
        }
    }

    fn set_closer(&self, conn: &DataConn) {
        let c = conn.closer();
        *self.shared.conn_closer.lock().unwrap() = Some(Box::new(c));
    }

    /// Resumes with backoff while the server still holds the stream.
    fn resume(&self, token: [u8; 16], received: u64, resume_ms: u32) -> Option<DataConn> {
        let give_up = Instant::now() + Duration::from_millis(u64::from(resume_ms.max(1000)));
        let mut delay = Duration::from_millis(200);
        while Instant::now() < give_up && !self.shared.closing.load(Ordering::Acquire) {
            match DataConn::resume(&self.block.endpoint, TERMINAL_V1, token, received) {
                Ok(c) => return Some(c),
                Err(DataError::Io(_)) => {}
                Err(_) => return None,
            }
            std::thread::sleep(delay);
            delay = (delay * 2).min(Duration::from_secs(5));
        }
        None
    }

    fn reattach(&self) {
        let _ = self.events.try_send(RexEvent::Reattach);
    }

    fn ended(&self, code: u16) {
        *self.shared.closed_code.lock().unwrap() = Some(code);
        let _ = self.events.try_send(RexEvent::BlockClosed);
        self.sink.finished();
    }

    fn deliver(&self, items: &mut Vec<ScanItem>) {
        for item in items.drain(..) {
            match item {
                ScanItem::Bytes(b) => {
                    self.sink.write(&b);
                }
                ScanItem::Message(m) => match m["type"].as_str() {
                    Some("resize") => {
                        if let (Some(c), Some(r)) = (m["cols"].as_u64(), m["rows"].as_u64()) {
                            self.sink.set_grid(c.min(u64::from(u16::MAX)) as u16, r.min(u64::from(u16::MAX)) as u16);
                        }
                    }
                    Some("theme") => {
                        let _ = self.events.try_send(RexEvent::Theme(m));
                    }
                    Some("child_exited") => {
                        let _ = self.events.try_send(RexEvent::Exited(m["exit_code"].as_i64().unwrap_or(0)));
                    }
                    _ => {}
                },
            }
        }
    }
}

/// `text` cut into pieces of at most `max` bytes, on character boundaries.
fn paste_chunks(text: &str, max: usize) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = text;
    while rest.len() > max {
        let mut at = max;
        while !rest.is_char_boundary(at) {
            at -= 1;
        }
        out.push(&rest[..at]);
        rest = &rest[at..];
    }
    if !rest.is_empty() || out.is_empty() {
        out.push(rest);
    }
    out
}

/// A pointer event for Rex's block.event: the wheel, or a button / motion at a cell (0-based).
/// `cell` is the cell size in the same device pixels as the input's position.
fn mouse_event(m: &MouseInput, cell: (u32, u32)) -> Option<serde_json::Value> {
    let at = |v: f32, size: u32| (v.max(0.0) / size.max(1) as f32).floor() as u32;
    let (x, y) = (at(m.x, cell.0), at(m.y, cell.1));
    let mods: Vec<&str> = [(m.modifiers.shift, "shift"), (m.modifiers.control, "ctrl"), (m.modifiers.alt, "alt"), (m.modifiers.super_key, "super")]
        .into_iter()
        .filter_map(|(on, name)| on.then_some(name))
        .collect();
    let mods = mods.join("+");
    let wheel = |dy: i32| json!({"type": "wheel", "payload": {"delta_x": 0, "delta_y": dy, "x": x, "y": y, "mods": mods}});
    let action = match m.action {
        MouseAction::ScrollUp => return Some(wheel(-1)),
        MouseAction::ScrollDown => return Some(wheel(1)),
        MouseAction::Press => "press",
        MouseAction::Release => "release",
        MouseAction::Move => "motion",
    };
    let button = match m.button {
        Some(MouseButton::Left) => "left",
        Some(MouseButton::Middle) => "middle",
        Some(MouseButton::Right) => "right",
        None => "none",
    };
    let mut buttons = serde_json::Map::new();
    if m.action != MouseAction::Release && button != "none" {
        buttons.insert(button.into(), json!(true));
    }
    Some(json!({"type": "mouse", "payload": {"action": action, "button": button, "buttons": buttons, "x": x, "y": y, "mods": mods}}))
}

struct RexHandle {
    block: BlockRef,
    shared: Arc<Shared>,
}

impl RexHandle {
    fn ids(&self) -> serde_json::Value {
        json!({"session_id": self.block.session_id, "block_id": self.block.block_id})
    }

    fn block_event(&self, event: serde_json::Value) {
        let mut p = self.ids();
        p["event"] = event;
        self.block.control.send("block.event", p);
    }
}

impl ByteHandle for RexHandle {
    /// Bytes the local engine would send to the program. Rex's own terminal already answers
    /// the program's queries and encodes keys, so these are dropped.
    fn write(&self, bytes: &[u8]) -> io::Result<usize> {
        Ok(bytes.len())
    }

    fn resize(&self, viewport: Viewport) -> io::Result<()> {
        *self.shared.cell.lock().unwrap() = (viewport.cell_width(), viewport.cell_height());
        let size = (viewport.columns(), viewport.rows());
        let generation = {
            let mut r = self.shared.resize.lock().unwrap();
            if r.1 == Some(size) {
                return Ok(());
            }
            r.0 += 1;
            r.1 = Some(size);
            r.0
        };
        let (shared, block) = (self.shared.clone(), self.block.clone());
        std::thread::spawn(move || {
            std::thread::sleep(RESIZE_DEBOUNCE);
            if shared.resize.lock().unwrap().0 != generation {
                return;
            }
            let ids = json!({"session_id": block.session_id, "block_id": block.block_id});
            let own = json!({"session_id": block.session_id, "block_ids": [block.block_id], "mode": "take"});
            block.control.send("session.own_block", own);
            let mut req = ids;
            req["args"] = json!({"columns": size.0, "rows": size.1});
            block.control.send("com.superlogical.terminal.resize", req);
        });
        Ok(())
    }

    fn try_wait(&self) -> io::Result<Option<ExitStatus>> {
        Ok(self.shared.closed_code.lock().unwrap().map(|_| ExitStatus::new(None, None::<String>)))
    }

    fn close(&self) {
        self.shared.closing.store(true, Ordering::Release);
        if let Some(c) = self.shared.conn_closer.lock().unwrap().as_ref() {
            c();
        }
    }

    fn route_input(&self, input: &TerminalInput) -> InputRoute {
        match input {
            TerminalInput::Key(k) => match keys::to_rex_key(k) {
                Some(key) => {
                    let mut p = self.ids();
                    p["key"] = key;
                    self.block.control.send("client.key", p);
                    InputRoute::Handled
                }
                None => InputRoute::Encode,
            },
            // An input method's commit. As a client.key it stays in order with the keys
            // around it (a block method such as terminal.write is not ordered with them).
            TerminalInput::Text(s) => {
                let mut p = self.ids();
                p["key"] = keys::text_key(s);
                self.block.control.send("client.key", p);
                InputRoute::Handled
            }
            // In pieces that fit the server's message limit (even if JSON escaping grows them
            // sixfold); block.event calls stay in order.
            TerminalInput::Paste(s) => {
                let max = (self.block.control.max_message_bytes() / 8).max(1024) as usize;
                for chunk in paste_chunks(s, max) {
                    self.block_event(json!({"type": "paste", "payload": {"text": chunk}}));
                }
                InputRoute::Handled
            }
            TerminalInput::Focus(on) => {
                self.block_event(json!({"type": "focus", "payload": {"action": if *on { "in" } else { "out" }}}));
                InputRoute::Handled
            }
            // The engine asks only while the program reports the mouse: Rex's terminal encodes it.
            TerminalInput::Mouse(m) => {
                let cell = *self.shared.cell.lock().unwrap();
                if let Some(ev) = mouse_event(m, cell) {
                    self.block_event(ev);
                }
                InputRoute::Handled
            }
            _ => InputRoute::Encode,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mouse_becomes_rex_events() {
        use gpui_cn::terminal::input::Modifiers;
        let cell = (10, 20);
        let up = MouseInput::new(MouseAction::ScrollUp, 55.0, 70.0);
        assert_eq!(mouse_event(&up, cell), Some(json!({"type": "wheel", "payload": {"delta_x": 0, "delta_y": -1, "x": 5, "y": 3, "mods": ""}})));
        let down = MouseInput::new(MouseAction::ScrollDown, 0.0, 0.0);
        assert_eq!(mouse_event(&down, cell).unwrap()["payload"]["delta_y"], 1);
        let mut press = MouseInput::new(MouseAction::Press, 31.0, 41.0).with_button(MouseButton::Left);
        let mut mods = Modifiers::default();
        mods.control = true;
        mods.alt = true;
        press.modifiers = mods;
        assert_eq!(
            mouse_event(&press, cell),
            Some(json!({"type": "mouse", "payload": {"action": "press", "button": "left", "buttons": {"left": true}, "x": 3, "y": 2, "mods": "ctrl+alt"}}))
        );
        let drag = MouseInput::new(MouseAction::Move, 45.0, 41.0).with_button(MouseButton::Right);
        assert_eq!(mouse_event(&drag, cell).unwrap()["payload"], json!({"action": "motion", "button": "right", "buttons": {"right": true}, "x": 4, "y": 2, "mods": ""}));
        let hover = MouseInput::new(MouseAction::Move, 5.0, 5.0);
        assert_eq!(mouse_event(&hover, cell).unwrap()["payload"], json!({"action": "motion", "button": "none", "buttons": {}, "x": 0, "y": 0, "mods": ""}));
        let release = MouseInput::new(MouseAction::Release, -3.0, 5.0).with_button(MouseButton::Middle);
        assert_eq!(mouse_event(&release, cell).unwrap()["payload"], json!({"action": "release", "button": "middle", "buttons": {}, "x": 0, "y": 0, "mods": ""}));
    }

    #[test]
    fn paste_chunks_respect_limit_and_char_boundaries() {
        let text = "中".repeat(1000) + &"a".repeat(1000);
        let chunks = paste_chunks(&text, 100);
        assert!(chunks.iter().all(|c| c.len() <= 100 && !c.is_empty()));
        assert_eq!(chunks.concat(), text);
        assert_eq!(paste_chunks("short", 100), vec!["short"]);
    }
}
