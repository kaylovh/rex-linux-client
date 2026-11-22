//! A Rex terminal block as a gpui-cn byte source.

mod attach;
pub mod keys;

pub use attach::{AttachError, Attached, BlockRef, attach};

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gpui_cn::terminal::{
    ByteHandle, ByteSink, ByteSource, Engine, EngineOptions, ExitStatus, InputRoute, TerminalInput, Viewport,
};
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
            let mut own = json!({"session_id": block.session_id, "block_ids": [block.block_id], "mode": "take"});
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
            TerminalInput::Text(s) => {
                let mut p = self.ids();
                p["args"] = json!({"data": s});
                self.block.control.send("com.superlogical.terminal.write", p);
                InputRoute::Handled
            }
            TerminalInput::Paste(s) => {
                self.block_event(json!({"type": "paste", "payload": {"text": s}}));
                InputRoute::Handled
            }
            TerminalInput::Focus(on) => {
                self.block_event(json!({"type": "focus", "payload": {"action": if *on { "in" } else { "out" }}}));
                InputRoute::Handled
            }
            // Scrolling is local; pointer reporting goes to the program in a later milestone.
            _ => InputRoute::Encode,
        }
    }
}
