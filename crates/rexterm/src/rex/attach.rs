//! Connecting to a block's terminal stream: a ticket, a data connection, and the snapshot.

use std::sync::Arc;

use rex_proto::control::{CallError, Control};
use rex_proto::data::{DataConn, DataError, DataItem, TERMINAL_V1};
use rex_proto::scan::split_snapshot;
use rex_proto::transport::Endpoint;

#[derive(Clone)]
pub struct BlockRef {
    pub endpoint: Endpoint,
    pub control: Arc<Control>,
    pub session_id: String,
    pub block_id: String,
}

pub struct Attached {
    /// The Ghostty snapshot the stream began with.
    pub snapshot: Vec<u8>,
    /// Stream bytes after the snapshot that were already read.
    pub rest: Vec<u8>,
    pub conn: DataConn,
}

#[derive(Debug)]
pub enum AttachError {
    Call(CallError),
    Data(DataError),
    Closed(u16),
    NoTicket,
}

impl std::fmt::Display for AttachError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AttachError::Call(e) => write!(f, "{e}"),
            AttachError::Data(e) => write!(f, "{e}"),
            AttachError::Closed(c) => write!(f, "terminal stream closed ({c})"),
            AttachError::NoTicket => f.write_str("no data ticket"),
        }
    }
}
impl std::error::Error for AttachError {}

/// Opens the block's terminal stream and reads up to the end of its snapshot. Blocks; call
/// it off the UI thread. The control connection must be attached to the block's session.
pub fn attach(block: &BlockRef) -> Result<Attached, AttachError> {
    let t = block
        .control
        .call(
            "data.connect",
            serde_json::json!({"session_id": block.session_id, "block_id": block.block_id, "data_ref_id": "terminal", "subprotocol": TERMINAL_V1}),
        )
        .map_err(AttachError::Call)?;
    let ticket = t["ticket"].as_str().ok_or(AttachError::NoTicket)?;
    let mut conn = DataConn::open(&block.endpoint, ticket, TERMINAL_V1).map_err(AttachError::Data)?;
    let mut buf = Vec::new();
    loop {
        match conn.next().map_err(|e| AttachError::Data(DataError::Io(e)))? {
            DataItem::Bytes(b) => buf.extend(b),
            DataItem::Closed(c) => return Err(AttachError::Closed(c)),
        }
        if let Some(at) = split_snapshot(&buf, conn.nonce) {
            let rest = buf.split_off(at);
            return Ok(Attached { snapshot: buf, rest, conn });
        }
    }
}
