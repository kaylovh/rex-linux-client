//! A small blocking WebSocket client (RFC 6455): no extensions, no compression.
//! Every frame the client sends is masked; pings are answered with pongs.

use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex};

/// A byte stream to a Rex server: its two directions and a way to unblock both.
pub struct Stream {
    pub reader: Box<dyn Read + Send>,
    pub writer: Box<dyn Write + Send>,
    /// Shuts the transport down so a blocked read returns. Safe to call more than once.
    pub closer: Arc<dyn Fn() + Send + Sync>,
}

/// One whole message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Text(String),
    Binary(Vec<u8>),
    Close { code: u16, reason: String },
}

/// The sending half. Clones share the connection.
#[derive(Clone)]
pub struct WsWriter {
    inner: Arc<Mutex<Box<dyn Write + Send>>>,
    closer: Arc<dyn Fn() + Send + Sync>,
}

/// The receiving half.
pub struct WsReader {
    reader: Box<dyn Read + Send>,
    writer: WsWriter,
}

const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
/// The largest message this client accepts (Rex data messages are at most 64 KiB; control
/// messages can be bigger, e.g. help.all).
const MAX_MESSAGE: u64 = 64 << 20;

fn accept_key(key: &str) -> String {
    use base64::Engine;
    use sha1::{Digest, Sha1};
    let mut h = Sha1::new();
    h.update(key.as_bytes());
    h.update(GUID.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(h.finalize())
}

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// Opens a WebSocket at `path` asking for `subprotocol`.
pub fn connect(stream: Stream, path: &str, subprotocol: &str) -> io::Result<(WsReader, WsWriter)> {
    use base64::Engine;
    let Stream { mut reader, writer, closer } = stream;
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce).map_err(|e| io::Error::other(e.to_string()))?;
    let key = base64::engine::general_purpose::STANDARD.encode(nonce);
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: rex\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: {subprotocol}\r\nUser-Agent: rexa-desktop\r\n\r\n"
    );
    let writer = WsWriter { inner: Arc::new(Mutex::new(writer)), closer };
    writer.inner.lock().unwrap().write_all(req.as_bytes())?;
    writer.inner.lock().unwrap().flush()?;

    // Read the response head a byte at a time, so no frame bytes are consumed.
    let mut head = Vec::new();
    let mut b = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if reader.read(&mut b)? == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "connection closed during the WebSocket handshake"));
        }
        head.push(b[0]);
        if head.len() > 16 * 1024 {
            return Err(bad("WebSocket handshake response too long"));
        }
    }
    let head = String::from_utf8_lossy(&head);
    let mut lines = head.split("\r\n");
    let status = lines.next().unwrap_or("");
    if status.split_whitespace().nth(1) != Some("101") {
        return Err(bad(format!("WebSocket upgrade refused: {status}")));
    }
    let want = accept_key(&key);
    let ok = lines.filter_map(|l| l.split_once(':')).any(|(k, v)| {
        k.trim().eq_ignore_ascii_case("sec-websocket-accept") && v.trim() == want
    });
    if !ok {
        return Err(bad("WebSocket handshake: wrong Sec-WebSocket-Accept"));
    }
    Ok((WsReader { reader, writer: writer.clone() }, writer))
}

impl WsWriter {
    fn frame(&self, op: u8, payload: &[u8]) -> io::Result<()> {
        let mut f = Vec::with_capacity(payload.len() + 14);
        f.push(0x80 | op);
        if payload.len() < 126 {
            f.push(0x80 | payload.len() as u8);
        } else if payload.len() <= 0xffff {
            f.push(0x80 | 126);
            f.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        } else {
            f.push(0x80 | 127);
            f.extend_from_slice(&(payload.len() as u64).to_be_bytes());
        }
        let mut mask = [0u8; 4];
        getrandom::fill(&mut mask).map_err(|e| io::Error::other(e.to_string()))?;
        f.extend_from_slice(&mask);
        f.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        let mut w = self.inner.lock().unwrap();
        w.write_all(&f)?;
        w.flush()
    }
    pub fn send_text(&self, text: &str) -> io::Result<()> {
        self.frame(0x1, text.as_bytes())
    }
    pub fn send_binary(&self, data: &[u8]) -> io::Result<()> {
        self.frame(0x2, data)
    }
    /// Sends a close frame with `code`.
    pub fn close(&self, code: u16) -> io::Result<()> {
        self.frame(0x8, &code.to_be_bytes())
    }
    /// Unblocks a reader on another thread.
    pub fn shutdown(&self) {
        (self.closer)()
    }
}

impl WsReader {
    /// Reads one frame: (fin, opcode, payload).
    fn frame(&mut self) -> io::Result<(bool, u8, Vec<u8>)> {
        let mut h = [0u8; 2];
        self.reader.read_exact(&mut h)?;
        let fin = h[0] & 0x80 != 0;
        let op = h[0] & 0x0f;
        let masked = h[1] & 0x80 != 0;
        let mut len = (h[1] & 0x7f) as u64;
        if len == 126 {
            let mut b = [0u8; 2];
            self.reader.read_exact(&mut b)?;
            len = u16::from_be_bytes(b) as u64;
        } else if len == 127 {
            let mut b = [0u8; 8];
            self.reader.read_exact(&mut b)?;
            len = u64::from_be_bytes(b);
        }
        if len > MAX_MESSAGE {
            return Err(bad("WebSocket frame too large"));
        }
        let mut mask = [0u8; 4];
        if masked {
            self.reader.read_exact(&mut mask)?;
        }
        let mut data = vec![0u8; len as usize];
        self.reader.read_exact(&mut data)?;
        if masked {
            for (i, b) in data.iter_mut().enumerate() {
                *b ^= mask[i % 4];
            }
        }
        Ok((fin, op, data))
    }

    pub fn read_message(&mut self) -> io::Result<Message> {
        let mut msg: Option<(u8, Vec<u8>)> = None;
        loop {
            let (fin, op, data) = self.frame()?;
            match op {
                0x9 => self.writer.frame(0xA, &data)?,
                0xA => {}
                0x8 => {
                    let code = if data.len() >= 2 { u16::from_be_bytes([data[0], data[1]]) } else { 1005 };
                    let reason = String::from_utf8_lossy(data.get(2..).unwrap_or(&[])).into_owned();
                    let _ = self.writer.close(code);
                    return Ok(Message::Close { code, reason });
                }
                0x0 => {
                    let Some((_, buf)) = msg.as_mut() else { return Err(bad("WebSocket continuation without a start")) };
                    if (buf.len() + data.len()) as u64 > MAX_MESSAGE {
                        return Err(bad("WebSocket message too large"));
                    }
                    buf.extend_from_slice(&data);
                }
                0x1 | 0x2 => {
                    if msg.is_some() {
                        return Err(bad("WebSocket message started inside another"));
                    }
                    msg = Some((op, data));
                }
                _ => return Err(bad(format!("unknown WebSocket opcode {op}"))),
            }
            if fin && matches!(op, 0x0 | 0x1 | 0x2) {
                let (op, data) = msg.take().unwrap();
                return Ok(if op == 0x1 {
                    Message::Text(String::from_utf8(data).map_err(|_| bad("WebSocket text is not UTF-8"))?)
                } else {
                    Message::Binary(data)
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixStream;
    use std::thread;

    const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

    fn pair() -> (Stream, UnixStream) {
        let (a, b) = UnixStream::pair().unwrap();
        let r = a.try_clone().unwrap();
        let c = a.try_clone().unwrap();
        let stream = Stream {
            reader: Box::new(r),
            writer: Box::new(a),
            closer: Arc::new(move || {
                let _ = c.shutdown(std::net::Shutdown::Both);
            }),
        };
        (stream, b)
    }

    /// The fake server: reads the request, answers it (with a wrong accept if asked).
    fn serve_handshake(peer: &UnixStream, wrong_accept: bool) -> Vec<String> {
        let mut rd = BufReader::new(peer.try_clone().unwrap());
        let mut lines = Vec::new();
        loop {
            let mut l = String::new();
            rd.read_line(&mut l).unwrap();
            if l == "\r\n" {
                break;
            }
            lines.push(l.trim_end().to_string());
        }
        let key = lines.iter().find_map(|l| l.strip_prefix("Sec-WebSocket-Key: ")).unwrap().to_string();
        let accept = if wrong_accept { "bogus".to_string() } else { accept_for(&key) };
        let resp = format!(
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\nSec-WebSocket-Protocol: rex.control.v1\r\n\r\n"
        );
        (&*peer).write_all(resp.as_bytes()).unwrap();
        lines
    }

    fn accept_for(key: &str) -> String {
        use base64::Engine;
        use sha1::{Digest, Sha1};
        let mut h = Sha1::new();
        h.update(key.as_bytes());
        h.update(GUID.as_bytes());
        base64::engine::general_purpose::STANDARD.encode(h.finalize())
    }

    /// Writes one unmasked server frame.
    fn server_frame(peer: &UnixStream, fin: bool, op: u8, payload: &[u8]) {
        let mut f = vec![if fin { 0x80 } else { 0 } | op];
        if payload.len() < 126 {
            f.push(payload.len() as u8);
        } else if payload.len() <= 0xffff {
            f.push(126);
            f.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        } else {
            f.push(127);
            f.extend_from_slice(&(payload.len() as u64).to_be_bytes());
        }
        f.extend_from_slice(payload);
        (&*peer).write_all(&f).unwrap();
    }

    /// Reads one client frame: (opcode, mask bit set, unmasked payload).
    fn read_client_frame(peer: &UnixStream) -> (u8, bool, Vec<u8>) {
        let mut p = peer;
        let mut h = [0u8; 2];
        p.read_exact(&mut h).unwrap();
        let masked = h[1] & 0x80 != 0;
        let mut len = (h[1] & 0x7f) as usize;
        if len == 126 {
            let mut b = [0u8; 2];
            p.read_exact(&mut b).unwrap();
            len = u16::from_be_bytes(b) as usize;
        } else if len == 127 {
            let mut b = [0u8; 8];
            p.read_exact(&mut b).unwrap();
            len = u64::from_be_bytes(b) as usize;
        }
        let mut mask = [0u8; 4];
        if masked {
            p.read_exact(&mut mask).unwrap();
        }
        let mut data = vec![0u8; len];
        p.read_exact(&mut data).unwrap();
        if masked {
            for (i, b) in data.iter_mut().enumerate() {
                *b ^= mask[i % 4];
            }
        }
        (h[0] & 0x0f, masked, data)
    }

    fn connected() -> (WsReader, WsWriter, UnixStream) {
        let (stream, peer) = pair();
        let p2 = peer.try_clone().unwrap();
        let t = thread::spawn(move || serve_handshake(&p2, false));
        let (r, w) = connect(stream, "/api/control", "rex.control.v1").unwrap();
        t.join().unwrap();
        (r, w, peer)
    }

    #[test]
    fn handshake_sends_exact_request_and_checks_accept() {
        let (stream, peer) = pair();
        let p2 = peer.try_clone().unwrap();
        let t = thread::spawn(move || serve_handshake(&p2, false));
        connect(stream, "/api/control", "rex.control.v1").unwrap();
        let lines = t.join().unwrap();
        assert_eq!(lines[0], "GET /api/control HTTP/1.1");
        assert_eq!(lines[1], "Host: rex");
        assert_eq!(lines[2], "Upgrade: websocket");
        assert_eq!(lines[3], "Connection: Upgrade");
        assert!(lines[4].starts_with("Sec-WebSocket-Key: "));
        assert_eq!(lines[5], "Sec-WebSocket-Version: 13");
        assert_eq!(lines[6], "Sec-WebSocket-Protocol: rex.control.v1");
        assert_eq!(lines[7], "User-Agent: rexa-desktop");

        let (stream, peer) = pair();
        let p2 = peer.try_clone().unwrap();
        let t = thread::spawn(move || serve_handshake(&p2, true));
        assert!(connect(stream, "/api/control", "rex.control.v1").is_err());
        t.join().unwrap();
        drop(peer);
    }

    #[test]
    fn client_frames_are_masked() {
        let (_r, w, peer) = connected();
        w.send_text("hi").unwrap();
        let (op, masked, data) = read_client_frame(&peer);
        assert_eq!(op, 1);
        assert!(masked);
        assert_eq!(data, b"hi");
    }

    #[test]
    fn reader_answers_ping_with_pong() {
        let (mut r, _w, peer) = connected();
        server_frame(&peer, true, 9, b"x");
        server_frame(&peer, true, 1, b"after");
        assert_eq!(r.read_message().unwrap(), Message::Text("after".into()));
        let (op, masked, data) = read_client_frame(&peer);
        assert_eq!((op, masked, data.as_slice()), (10, true, &b"x"[..]));
    }

    #[test]
    fn fragmented_message_is_joined() {
        let (mut r, _w, peer) = connected();
        server_frame(&peer, false, 2, b"ab");
        server_frame(&peer, true, 0, b"cd");
        assert_eq!(r.read_message().unwrap(), Message::Binary(b"abcd".to_vec()));
    }

    #[test]
    fn length_126_and_127_parse() {
        let (mut r, _w, peer) = connected();
        let big = vec![7u8; 70000];
        let mid = vec![3u8; 300];
        let p2 = peer.try_clone().unwrap();
        let (b2, m2) = (big.clone(), mid.clone());
        let t = thread::spawn(move || {
            server_frame(&p2, true, 2, &m2);
            server_frame(&p2, true, 2, &b2);
        });
        assert_eq!(r.read_message().unwrap(), Message::Binary(mid));
        assert_eq!(r.read_message().unwrap(), Message::Binary(big));
        t.join().unwrap();
    }

    #[test]
    fn close_frame_yields_code() {
        let (mut r, _w, peer) = connected();
        let mut p = 1000u16.to_be_bytes().to_vec();
        p.extend_from_slice(b"bye");
        server_frame(&peer, true, 8, &p);
        assert_eq!(r.read_message().unwrap(), Message::Close { code: 1000, reason: "bye".into() });
    }
}
