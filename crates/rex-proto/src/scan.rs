//! Splits a terminal stream into VT bytes and Rex's own in-band messages
//! (`ESC _ rex1;<nonce>;<json> ESC \\`), keeping their order across chunk boundaries.
//! APCs that aren't ours (wrong nonce, other protocols) stay in the byte stream.

use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum ScanItem {
    Bytes(Vec<u8>),
    Message(Value),
}

const ESC: u8 = 0x1b;
const MAX_APC: usize = 1 << 20;

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Normal,
    Esc,
    Apc,
}

pub struct Scanner {
    nonce: String,
    state: State,
    apc: Vec<u8>,
    apc_esc: bool,
}

impl Scanner {
    pub fn new(nonce: u64) -> Scanner {
        Scanner { nonce: nonce.to_string(), state: State::Normal, apc: Vec::new(), apc_esc: false }
    }

    /// Splits `chunk` into items appended to `out`. An ESC at the end of a chunk is held until
    /// the next one says whether it starts an APC.
    pub fn feed(&mut self, chunk: &[u8], out: &mut Vec<ScanItem>) {
        let mut bytes: Vec<u8> = Vec::new();
        for &b in chunk {
            match self.state {
                State::Normal => {
                    if b == ESC {
                        self.state = State::Esc;
                    } else {
                        bytes.push(b);
                    }
                }
                State::Esc => {
                    if b == b'_' {
                        self.state = State::Apc;
                        self.apc.clear();
                        self.apc_esc = false;
                    } else {
                        bytes.push(ESC);
                        if b != ESC {
                            bytes.push(b);
                            self.state = State::Normal;
                        }
                    }
                }
                State::Apc => {
                    if self.apc_esc && b == b'\\' {
                        self.finish_apc(&mut bytes, out);
                        self.state = State::Normal;
                        continue;
                    }
                    if self.apc_esc {
                        self.apc.push(ESC);
                    }
                    self.apc_esc = b == ESC;
                    if !self.apc_esc {
                        self.apc.push(b);
                    }
                    if self.apc.len() > MAX_APC {
                        bytes.extend_from_slice(b"\x1b_");
                        bytes.append(&mut self.apc);
                        self.state = State::Normal;
                    }
                }
            }
        }
        if !bytes.is_empty() {
            out.push(ScanItem::Bytes(bytes));
        }
    }

    fn finish_apc(&mut self, bytes: &mut Vec<u8>, out: &mut Vec<ScanItem>) {
        let body = std::mem::take(&mut self.apc);
        if let Some(rest) = body.strip_prefix(b"rex1;")
            && let Some(semi) = rest.iter().position(|&c| c == b';')
            && rest[..semi] == *self.nonce.as_bytes()
            && let Ok(v) = serde_json::from_slice::<Value>(&rest[semi + 1..])
        {
            if !bytes.is_empty() {
                out.push(ScanItem::Bytes(std::mem::take(bytes)));
            }
            out.push(ScanItem::Message(v));
            return;
        }
        bytes.extend_from_slice(b"\x1b_");
        bytes.extend_from_slice(&body);
        bytes.extend_from_slice(b"\x1b\\");
    }
}

/// Where the snapshot at the start of a terminal stream ends: the first `ESC _ rex1;<nonce>;`.
pub fn split_snapshot(buf: &[u8], nonce: u64) -> Option<usize> {
    let marker = format!("\x1b_rex1;{nonce};");
    buf.windows(marker.len()).position(|w| w == marker.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const INPUT: &[u8] = b"ab\x1b_rex1;7;{\"type\":\"resize\",\"cols\":80,\"rows\":24}\x1b\\cd";

    /// Joins neighbouring Bytes, so results from different chunkings compare equal.
    fn normalize(items: Vec<ScanItem>) -> Vec<ScanItem> {
        let mut out: Vec<ScanItem> = Vec::new();
        for it in items {
            match (out.last_mut(), it) {
                (Some(ScanItem::Bytes(a)), ScanItem::Bytes(b)) => a.extend(b),
                (_, ScanItem::Bytes(b)) if b.is_empty() => {}
                (_, it) => out.push(it),
            }
        }
        out
    }

    fn scan(nonce: u64, chunks: &[&[u8]]) -> Vec<ScanItem> {
        let mut s = Scanner::new(nonce);
        let mut out = Vec::new();
        for c in chunks {
            s.feed(c, &mut out);
        }
        normalize(out)
    }

    #[test]
    fn scanner_extracts_message() {
        assert_eq!(
            scan(7, &[INPUT]),
            vec![
                ScanItem::Bytes(b"ab".to_vec()),
                ScanItem::Message(json!({"type": "resize", "cols": 80, "rows": 24})),
                ScanItem::Bytes(b"cd".to_vec()),
            ]
        );
    }

    #[test]
    fn scanner_keeps_foreign_apc() {
        assert_eq!(scan(8, &[INPUT]), vec![ScanItem::Bytes(INPUT.to_vec())]);
        let other = b"x\x1b_Gf=24;AAAA\x1b\\y";
        assert_eq!(scan(7, &[other]), vec![ScanItem::Bytes(other.to_vec())]);
    }

    #[test]
    fn scanner_keeps_plain_escapes() {
        let vt = b"\x1b[31mred\x1b[0m\x1b\x1b[A";
        assert_eq!(scan(7, &[vt]), vec![ScanItem::Bytes(vt.to_vec())]);
    }

    #[test]
    fn scanner_splits_at_every_offset() {
        let whole = scan(7, &[INPUT]);
        for i in 0..=INPUT.len() {
            assert_eq!(scan(7, &[&INPUT[..i], &INPUT[i..]]), whole, "split at {i}");
        }
    }

    #[test]
    fn snapshot_ends_at_first_marker() {
        assert_eq!(split_snapshot(INPUT, 7), Some(2));
        assert_eq!(split_snapshot(INPUT, 8), None);
        assert_eq!(split_snapshot(b"\x1b_rex1;7", 7), None);
    }
}
