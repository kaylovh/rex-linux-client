//! The control connection (`rex.control.v1`): hello/welcome, requests and responses with
//! string ids, and pushed events. A reader thread hands responses to waiting callers and
//! events to a channel, and reconnects (resuming this client) when the connection drops.

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use serde_json::{Value, json};

use crate::transport::Endpoint;
use crate::ws::{self, Message, WsReader, WsWriter};

#[derive(Debug, Clone, PartialEq)]
pub enum CallError {
    Remote { code: String, message: String, status: i64 },
    Closed,
    Timeout,
}

#[derive(Debug)]
pub enum ConnectError {
    Io(io::Error),
    NoWelcome(String),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::Remote { code, message, .. } => write!(f, "{code}: {message}"),
            CallError::Closed => f.write_str("control connection closed"),
            CallError::Timeout => f.write_str("request timed out"),
        }
    }
}
impl std::error::Error for CallError {}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::Io(e) => write!(f, "{e}"),
            ConnectError::NoWelcome(m) => write!(f, "the server did not accept the connection: {m}"),
        }
    }
}
impl std::error::Error for ConnectError {}

impl From<io::Error> for ConnectError {
    fn from(e: io::Error) -> Self {
        ConnectError::Io(e)
    }
}

/// How long a call waits for its response.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

type Reply = Result<Value, CallError>;

pub struct Control {
    ep: Endpoint,
    name: String,
    writer: Mutex<WsWriter>,
    client_id: Mutex<String>,
    token: Mutex<String>,
    pending: Mutex<HashMap<u64, mpsc::Sender<Reply>>>,
    next_id: AtomicU64,
    closing: AtomicBool,
    events_tx: async_channel::Sender<Value>,
    events_rx: async_channel::Receiver<Value>,
}

struct Welcome {
    reader: WsReader,
    writer: WsWriter,
    client_id: String,
    token: String,
    resumed: bool,
}

fn platform() -> &'static str {
    if cfg!(target_os = "macos") { "macos" } else { "linux" }
}

fn handshake(ep: &Endpoint, name: &str, token: Option<&str>) -> Result<Welcome, ConnectError> {
    let stream = ep.connect(CONNECT_TIMEOUT)?;
    let (mut reader, writer) = ws::connect(stream, "/api/control", "rex.control.v1")?;
    let client = json!({"kind": "app", "platform": platform(), "name": name, "version": env!("CARGO_PKG_VERSION")});
    let mut hello = json!({"type": "hello", "client": client, "api_revision": crate::API_REVISION});
    if let Some(t) = token {
        hello["reconnect_token"] = json!(t);
    }
    writer.send_text(&hello.to_string())?;
    let msg = match reader.read_message()? {
        Message::Text(t) => t,
        Message::Close { code, reason } => return Err(ConnectError::NoWelcome(format!("closed {code} {reason}"))),
        Message::Binary(_) => return Err(ConnectError::NoWelcome("binary reply".into())),
    };
    let v: Value = serde_json::from_str(&msg).map_err(|e| ConnectError::NoWelcome(e.to_string()))?;
    if v["type"] != "welcome" || v["protocol"] != "rex.control.v1" {
        return Err(ConnectError::NoWelcome(msg));
    }
    let client = &v["client"];    let client_id = client["id"].as_str().ok_or_else(|| ConnectError::NoWelcome(msg.clone()))?.to_string();
    Ok(Welcome {
        reader,
        writer,
        client_id,
        token: client["reconnect_token"].as_str().unwrap_or("").to_string(),
        resumed: client["resumed"].as_bool().unwrap_or(false),
    })
}

impl Control {
    pub fn connect(ep: Endpoint, client_name: &str) -> Result<Arc<Control>, ConnectError> {
        let w = handshake(&ep, client_name, None)?;
        let (events_tx, events_rx) = async_channel::unbounded();
        let ctl = Arc::new(Control {
            ep,
            name: client_name.to_string(),
            writer: Mutex::new(w.writer),
            client_id: Mutex::new(w.client_id),
            token: Mutex::new(w.token),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            closing: AtomicBool::new(false),
            events_tx,
            events_rx,
        });
        let c = ctl.clone();
        let reader = w.reader;
        std::thread::Builder::new().name("rex-control".into()).spawn(move || c.read_loop(reader))?;
        Ok(ctl)
    }

    fn send_request(&self, id: u64, method: &str, payload: Value) -> io::Result<()> {
        let mut req = json!({"type": "request", "id": format!("x{id}"), "method": method});
        if !payload.is_null() {
            req["payload"] = payload;
        }
        let w = self.writer.lock().unwrap().clone();
        w.send_text(&req.to_string())
    }

    /// Blocking call; the response's payload, or why there is none.
    pub fn call(&self, method: &str, payload: Value) -> Result<Value, CallError> {
        let id = self.next_id.fetchadd();
        let (tx, rx) = mpsc::channel();
        self.pending.lock().unwrap().insert(id, tx);
        if self.send_request(id, method, payload).is_err() {
            self.pending.lock().unwrap().remove(&id);
            return Err(CallError::Closed);
        }
        match rx.recv_timeout(CALL_TIMEOUT) {
            Ok(r) => r,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.pending.lock().unwrap().remove(&id);
                Err(CallError::Timeout)
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(CallError::Closed),
        }
    }

    /// Fire and forget. Ordered methods (client.key, block.event, ...) still run in send
    /// order on the server.
    pub fn send(&self, method: &str, payload: Value) {
        let id = self.next_id.fetchadd();
        let _ = self.send_request(id, method, payload);
    }

    /// Pushed events (the `payload` of each event message: `{"type": name, "payload": ...}`;
    /// session events also carry `session_id`),
    /// plus `rexa_reconnecting`, `rexa_reconnected` and `rexa_disconnected`.
    pub fn events(&self) -> async_channel::Receiver<Value> {
        self.events_rx.clone()
    }

    pub fn client_id(&self) -> String {
        self.client_id.lock().unwrap().clone()
    }

    pub fn close(&self) {
        self.closing.store(true, Ordering::Release);
        let w = self.writer.lock().unwrap().clone();
        let _ = w.close(1000);
        w.shutdown();
    }

    #[cfg(feature = "test-hooks")]
    pub fn drop_connection_for_test(&self) {
        self.writer.lock().unwrap().shutdown();
    }

    fn emit(&self, ev: Value) {
        let _ = self.events_tx.try_send(ev);
    }

    fn fail_pending(&self) {
        for (_, tx) in self.pending.lock().unwrap().drain() {
            let _ = tx.send(Err(CallError::Closed));
        }
    }

    fn read_loop(self: Arc<Self>, mut reader: WsReader) {
        loop {
            self.read_until_closed(&mut reader);
            self.fail_pending();
            if self.closing.load(Ordering::Acquire) {
                break;
            }
            match self.reconnect() {
                Some(r) => reader = r,
                None => break,
            }
        }
        self.emit(json!({"type": "rexa_disconnected"}));
    }

    fn read_until_closed(&self, reader: &mut WsReader) {
        loop {
            let text = match reader.read_message() {
                Ok(Message::Text(t)) => t,
                Ok(Message::Binary(_)) => continue,
                Ok(Message::Close { .. }) | Err(_) => return,
            };
            let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
            match v["type"].as_str() {
                Some("response") => {
                    let Some(id) = v["id"].as_str().and_then(|s| s.strip_prefix('x')).and_then(|s| s.parse::<u64>().ok()) else {
                        continue;
                    };
                    let Some(tx) = self.pending.lock().unwrap().remove(&id) else { continue };
                    let reply = if let Some(e) = v.get("error").filter(|e| e.is_object()) {
                        Err(CallError::Remote {
                            code: e["code"].as_str().unwrap_or("error").to_string(),
                            message: e["message"].as_str().unwrap_or("").to_string(),
                            status: e["status"].as_i64().unwrap_or(0),
                        })
                    } else {
                        Ok(v.get("payload").cloned().unwrap_or(Value::Null))
                    };
                    let _ = tx.send(reply);
                }
                Some("event") => self.emit(flatten_event(v.get("payload").cloned().unwrap_or(Value::Null))),
                _ => {}
            }
        }
    }

    /// Tries again, resuming this client, until it works or close() is called.
    fn reconnect(&self) -> Option<WsReader> {
        self.emit(json!({"type": "rexa_reconnecting"}));
        let mut delay = Duration::from_millis(200);
        while !self.closing.load(Ordering::Acquire) {
            std::thread::sleep(delay);
            if self.closing.load(Ordering::Acquire) {
                return None;
            }
            let token = self.token.lock().unwrap().clone();
            let w = match handshake(&self.ep, &self.name, Some(&token)) {
                Ok(w) => w,
                Err(_) => {
                    delay = (delay * 2).min(Duration::from_secs(5));
                    continue;
                }
            };
            let changed = *self.client_id.lock().unwrap() != w.client_id;
            *self.writer.lock().unwrap() = w.writer;
            *self.token.lock().unwrap() = w.token;
            *self.client_id.lock().unwrap() = w.client_id;
            self.emit(json!({"type": "rexa_reconnected", "resumed": w.resumed && !changed}));
            return Some(w.reader);
        }
        None
    }
}

/// A session's event arrives as `{"type":"session_event","payload":{"session_id","event":{"type","payload"}}}`;
/// hand it on as `{"type": <name>, "session_id", "payload"}` so callers match on one name.
fn flatten_event(ev: Value) -> Value {
    if ev["type"] == "session_event" {
        let p = &ev["payload"];
        return json!({
            "type": p["event"]["type"].clone(),
            "session_id": p["session_id"].clone(),
            "payload": p["event"]["payload"].clone(),
        });
    }
    ev
}

trait FetchAdd {
    fn fetchadd(&self) -> u64;
}
impl FetchAdd for AtomicU64 {
    fn fetchadd(&self) -> u64 {
        self.fetch_add(1, Ordering::Relaxed)
    }
}
