//! Tests against a real, isolated Rex server (see rex_proto::testing).

use std::time::{Duration, Instant};

use rex_proto::control::{CallError, Control};
use rex_proto::data::{DataConn, DataItem, TERMINAL_V1};
use rex_proto::scan::{ScanItem, Scanner, split_snapshot};
use rex_proto::testing::TestServer;
use serde_json::{Value, json};

fn next_event(rx: &async_channel::Receiver<Value>, name: &str, within: Duration) -> Option<Value> {
    let start = Instant::now();
    while start.elapsed() < within {
        match rx.try_recv() {
            Ok(ev) if ev["type"] == name => return Some(ev),
            Ok(_) => {}
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    None
}

#[test]
fn control_hello_and_call() {
    let Some(srv) = TestServer::start() else { return };
    let ctl = Control::connect(srv.endpoint.clone(), "rexa-desktop-test").unwrap();
    let v = ctl.call("server.version", json!({})).unwrap();
    assert!(v["rex_version"].is_string(), "{v}");
    ctl.close();
}

#[test]
fn events_arrive_after_attach() {
    let Some(srv) = TestServer::start() else { return };
    let ctl = Control::connect(srv.endpoint.clone(), "rexa-desktop-test").unwrap();
    let created = ctl.call("session.create", json!({})).unwrap();
    let sid = created["session_id"].as_str().unwrap().to_string();
    ctl.call("session.attach", json!({"session_id": sid})).unwrap();
    ctl.call("session.set_label", json!({"session_id": sid, "label": "renamed"})).unwrap();
    let ev = next_event(&ctl.events(), "session_label_changed", Duration::from_secs(10)).expect("no session_label_changed");
    assert_eq!(ev["payload"]["label"], "renamed");
}

#[test]
fn remote_error_maps_to_call_error() {
    let Some(srv) = TestServer::start() else { return };
    let ctl = Control::connect(srv.endpoint.clone(), "rexa-desktop-test").unwrap();
    match ctl.call("session.view", json!({})) {
        Err(CallError::Remote { code, .. }) => assert_eq!(code, "bad_request"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn reconnect_resumes_same_client() {
    let Some(srv) = TestServer::start() else { return };
    let ctl = Control::connect(srv.endpoint.clone(), "rexa-desktop-test").unwrap();
    let sid = ctl.call("session.create", json!({})).unwrap()["session_id"].as_str().unwrap().to_string();
    ctl.call("session.attach", json!({"session_id": sid})).unwrap();
    let id = ctl.client_id();
    let rx = ctl.events();
    ctl.drop_connection_for_test();
    next_event(&rx, "rexa_reconnecting", Duration::from_secs(5)).expect("no rexa_reconnecting");
    let ev = next_event(&rx, "rexa_reconnected", Duration::from_secs(10)).expect("no rexa_reconnected");
    assert_eq!(ev["resumed"], true);
    assert_eq!(ctl.client_id(), id);
    assert!(ctl.call("server.version", json!({})).is_ok());
}

/// A session with one terminal block, attached on `ctl`: (session id, block id).
fn session_with_block(ctl: &Control) -> (String, String) {
    let created = ctl.call("session.create", json!({"initial_windows": [{"layout": {"block": {"flavor": "com.superlogical.terminal.shell"}}}]})).unwrap();
    let sid = created["session_id"].as_str().unwrap().to_string();
    let att = ctl.call("session.attach", json!({"session_id": sid})).unwrap();
    let view = &att["view"];
    let active = view["active_window_id"].as_str().unwrap();
    let win = view["windows"].as_array().unwrap().iter().find(|w| w["window_id"] == active).unwrap();
    let bid = win["focused_block_id"].as_str().unwrap().to_string();
    (sid, bid)
}

fn write_to(ctl: &Control, sid: &str, bid: &str, data: &str) {
    ctl.call("com.superlogical.terminal.write", json!({"session_id": sid, "block_id": bid, "args": {"data": data}})).unwrap();
}

fn open_terminal(ctl: &Control, ep: &rex_proto::transport::Endpoint, sid: &str, bid: &str) -> DataConn {
    let t = ctl
        .call("data.connect", json!({"session_id": sid, "block_id": bid, "data_ref_id": "terminal", "subprotocol": TERMINAL_V1}))
        .unwrap();
    DataConn::open(ep, t["ticket"].as_str().unwrap(), TERMINAL_V1).unwrap()
}

/// Reads until the snapshot is complete: (snapshot bytes, the bytes after it).
fn read_snapshot(dc: &mut DataConn) -> (Vec<u8>, Vec<u8>) {
    let mut buf = Vec::new();
    loop {
        match dc.next().unwrap() {
            DataItem::Bytes(b) => buf.extend(b),
            DataItem::Closed(c) => panic!("stream closed {c}"),
        }
        if let Some(at) = split_snapshot(&buf, dc.nonce) {
            let rest = buf.split_off(at);
            return (buf, rest);
        }
    }
}

fn screen_text(t: &ghostty_vt::Terminal) -> String {
    use ghostty_vt::fmt::{Format, Formatter, FormatterOptions};
    let mut f = Formatter::new(t, FormatterOptions::new().with_format(Format::Plain)).unwrap();
    String::from_utf8_lossy(&f.format_alloc().unwrap()).into_owned()
}

#[test]
fn snapshot_decodes_with_ghostty_vt() {
    let Some(srv) = TestServer::start() else { return };
    let ctl = Control::connect(srv.endpoint.clone(), "rexa-desktop-test").unwrap();
    let (sid, bid) = session_with_block(&ctl);
    std::thread::sleep(Duration::from_millis(500));
    write_to(&ctl, &sid, &bid, "echo REXA_M0_MARK\n");
    std::thread::sleep(Duration::from_millis(800));
    let mut dc = open_terminal(&ctl, &srv.endpoint, &sid, &bid);
    let (snap, _rest) = read_snapshot(&mut dc);
    let term = ghostty_vt::snapshot::Decoder::from_slice(&snap).decode().expect("ghostty-vt could not decode the Rex snapshot");
    let size = ctl.call("com.superlogical.terminal.size", json!({"session_id": sid, "block_id": bid, "args": {}})).unwrap();
    assert_eq!(term.cols().unwrap() as i64, size["columns"].as_i64().unwrap());
    assert_eq!(term.rows().unwrap() as i64, size["rows"].as_i64().unwrap());
    let text = screen_text(&term);
    assert!(text.contains("REXA_M0_MARK"), "screen was: {text}");
}

#[test]
fn resume_continues_without_gap() {
    let Some(srv) = TestServer::start() else { return };
    let ctl = Control::connect(srv.endpoint.clone(), "rexa-desktop-test").unwrap();
    let (sid, bid) = session_with_block(&ctl);
    let mut dc = open_terminal(&ctl, &srv.endpoint, &sid, &bid);
    let (_snap, rest) = read_snapshot(&mut dc);
    let mut scanner = Scanner::new(dc.nonce);
    let mut items = Vec::new();
    scanner.feed(&rest, &mut items);
    // The quotes keep the markers out of the echoed (and redrawn) command line.
    write_to(&ctl, &sid, &bid, "echo SEQ_''START; seq 1 200000; echo SEQ_''DONE\n");
    let mut text = Vec::new();
    let mut dropped = false;
    let start = Instant::now();
    loop {
        assert!(start.elapsed() < Duration::from_secs(60), "timed out");
        let item = match dc.next() {
            Ok(i) => i,
            Err(_) => DataItem::Closed(1006),
        };
        match item {
            DataItem::Bytes(b) => scanner.feed(&b, &mut items),
            DataItem::Closed(_) => {
                let (token, received) = (dc.token, dc.received);
                dc = DataConn::resume(&srv.endpoint, TERMINAL_V1, token, received).unwrap();
                continue;
            }
        }
        for it in items.drain(..) {
            if let ScanItem::Bytes(b) = it {
                text.extend(b);
            }
        }
        let s = String::from_utf8_lossy(&text);
        if !dropped && s.contains("SEQ_START") && s.contains("100000") {
            dc.shutdown();
            dropped = true;
        }
        if s.contains("SEQ_DONE") {
            break;
        }
    }
    assert!(dropped, "never dropped the connection");
    let s = String::from_utf8_lossy(&text);
    let s = &s[s.find("SEQ_START").unwrap() + "SEQ_START".len()..s.find("SEQ_DONE").unwrap()];
    let nums: Vec<u64> = s.split(|c: char| !c.is_ascii_digit()).filter_map(|w| w.parse().ok()).filter(|n| (1..=200000).contains(n)).collect();
    let first = nums.iter().position(|&n| n == 1).expect("no 1");
    let seq: Vec<u64> = nums[first..].iter().copied().take(200000).collect();
    assert_eq!(seq.len(), 200000);
    for (i, n) in seq.iter().enumerate() {
        assert_eq!(*n, i as u64 + 1, "gap or duplicate at {i}");
    }
}
