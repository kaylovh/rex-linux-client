//! Data connections (`GET /api/data`): a binary HELLO/WELCOME/REJECT/RESUME handshake,
//! then a one-way stream from the server (terminal output) or, for uploads, to it.

use std::io;
use std::time::Duration;

use crate::transport::Endpoint;
use crate::ws::{self, Message, WsReader, WsWriter};

pub const TERMINAL_V1: &str = "rex.data.terminal.v1";
pub const BYTES_V1: &str = "rex.data.bytes.v1";

#[derive(Debug)]
pub enum DataError {
    TicketInvalid,
    AlreadyConnected,
    StreamUnavailable,
    Limit,
    Malformed,
    ServerFailed,
    WrongSubprotocol,
    Unknown(u8),
    BadWelcome,
    Io(io::Error),
}


impl std::fmt::Display for DataError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DataError::TicketInvalid => f.write_str("data ticket or token not accepted"),
            DataError::AlreadyConnected => f.write_str("already connected to this stream"),
            DataError::StreamUnavailable => f.write_str("stream not available"),
            DataError::Limit => f.write_str("too many data connections"),
            DataError::Malformed => f.write_str("malformed data handshake"),
            DataError::ServerFailed => f.write_str("server failed"),
            DataError::WrongSubprotocol => f.write_str("wrong data subprotocol"),
            DataError::Unknown(c) => write!(f, "data connection refused ({c:#04x})"),
            DataError::BadWelcome => f.write_str("bad data welcome"),
            DataError::Io(e) => write!(f, "{e}"),
        }
    }
}
impl std::error::Error for DataError {}

impl From<io::Error> for DataError {
    fn from(e: io::Error) -> Self {
        DataError::Io(e)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataItem {
    Bytes(Vec<u8>),
    Closed(u16),
}

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

pub struct DataConn {
    pub token: [u8; 16],
    pub nonce: u64,
    pub resume_ms: u32,
    /// Stream bytes handed to the caller so far, across every connection of the stream.
    pub received: u64,
    reader: WsReader,
    writer: WsWriter,
}

/// The WELCOME's fields: (token, resume_ms, nonce).
fn parse_welcome(msg: &[u8]) -> Result<([u8; 16], u32, u64), DataError> {
    if msg.len() != 32 || msg[0] != 0x81 {
        return Err(DataError::BadWelcome);
    }
    let mut token = [0u8; 16];
    token.copy_from_slice(&msg[1..17]);
    let resume_ms = u32::from_le_bytes(msg[20..24].try_into().unwrap());
    let nonce = u64::from_le_bytes(msg[24..32].try_into().unwrap());
    Ok((token, resume_ms, nonce))
}

fn reject_reason(code: u8) -> DataError {
    match code {
        0x01 => DataError::TicketInvalid,
        0x02 => DataError::AlreadyConnected,
        0x03 => DataError::StreamUnavailable,
        0x04 => DataError::Limit,
        0x05 => DataError::Malformed,
        0x06 => DataError::ServerFailed,
        0x07 => DataError::WrongSubprotocol,
        c => DataError::Unknown(c),
    }
}

impl DataConn {
    pub fn open(ep: &Endpoint, ticket_b64: &str, subprotocol: &str) -> Result<DataConn, DataError> {
        use base64::Engine;
        let ticket = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(ticket_b64.trim_end_matches('='))
            .map_err(|_| DataError::TicketInvalid)?;
        if ticket.len() != 16 {
            return Err(DataError::TicketInvalid);
        }
        let mut hello = vec![0x01];
        hello.extend_from_slice(&ticket);
        Self::handshake(ep, subprotocol, &hello, 0)
    }

    /// Continues a dropped stream: RESUME with the latest token and byte count.
    pub fn resume(ep: &Endpoint, subprotocol: &str, token: [u8; 16], received: u64) -> Result<DataConn, DataError> {
        let mut msg = vec![0x02];
        msg.extend_from_slice(&token);
        msg.extend_from_slice(&received.to_le_bytes());
        Self::handshake(ep, subprotocol, &msg, received)
    }

    fn handshake(ep: &Endpoint, subprotocol: &str, first: &[u8], received: u64) -> Result<DataConn, DataError> {
        let stream = ep.connect(CONNECT_TIMEOUT)?;
        let (mut reader, writer) = ws::connect(stream, "/api/data", subprotocol)?;
        writer.send_binary(first)?;
        let msg = match reader.read_message()? {
            Message::Binary(b) => b,
            Message::Close { .. } => return Err(DataError::StreamUnavailable),
            Message::Text(_) => return Err(DataError::BadWelcome),
        };
        if msg.len() == 2 && msg[0] == 0x82 {
            writer.shutdown();
            return Err(reject_reason(msg[1]));
        }
        let (token, resume_ms, nonce) = parse_welcome(&msg)?;
        Ok(DataConn { token, nonce, resume_ms, received, reader, writer })
    }

    /// The next chunk of the stream, or how it closed (1006 when the connection dropped).
    pub fn next(&mut self) -> io::Result<DataItem> {
        loop {
            match self.reader.read_message() {
                Ok(Message::Binary(b)) => {
                    self.received += b.len() as u64;
                    return Ok(DataItem::Bytes(b));
                }
                Ok(Message::Close { code, .. }) => return Ok(DataItem::Closed(code)),
                Ok(Message::Text(_)) => continue,
                Err(e) if matches!(e.kind(), io::ErrorKind::InvalidData) => return Err(e),
                Err(_) => return Ok(DataItem::Closed(1006)),
            }
        }
    }

    pub fn send(&self, bytes: &[u8]) -> io::Result<()> {
        self.writer.send_binary(bytes)
    }

    /// Unblocks a reader in next() from another thread; the stream waits for a resume.
    pub fn shutdown(&self) {
        self.writer.shutdown();
    }

    /// A function that does what [`shutdown`](Self::shutdown) does, for another thread to keep.
    pub fn closer(&self) -> impl Fn() + Send + 'static {
        let w = self.writer.clone();
        move || w.shutdown()
    }

    /// Ends the stream for good (status 1000).
    pub fn close(self) {
        let _ = self.writer.close(1000);
        self.writer.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn welcome_parses_fields() {
        let mut m = vec![0x81];
        m.extend(1..=16u8);
        m.extend([0, 0, 0]);
        m.extend(60000u32.to_le_bytes());
        m.extend(0x1122334455667788u64.to_le_bytes());
        let (token, resume_ms, nonce) = parse_welcome(&m).unwrap();
        assert_eq!(token[0], 1);
        assert_eq!(token[15], 16);
        assert_eq!(resume_ms, 60000);
        assert_eq!(nonce, 0x1122334455667788);
        assert!(matches!(parse_welcome(&m[..31]), Err(DataError::BadWelcome)));
    }

    #[test]
    fn reject_reason_maps() {
        assert!(matches!(reject_reason(0x01), DataError::TicketInvalid));
        assert!(matches!(reject_reason(0x03), DataError::StreamUnavailable));
        assert!(matches!(reject_reason(0x07), DataError::WrongSubprotocol));
        assert!(matches!(reject_reason(0x09), DataError::Unknown(9)));
    }
}
