//! Terminal views for Rex blocks.

#[cfg(test)]
mod tests {
    #[test]
    fn ghostty_snapshot_roundtrip() {
        let mut t = ghostty_vt::Terminal::new(ghostty_vt::TerminalOptions { cols: 20, rows: 3, ..Default::default() }).unwrap();
        t.vt_write(b"hello");
        let bytes = ghostty_vt::snapshot::encode(&t).unwrap();
        let back = ghostty_vt::snapshot::Decoder::from_slice(&bytes).decode().unwrap();
        assert_eq!(back.cursor_x().unwrap(), 5);
    }
}
