//! A Rex block in a gpui-cn engine, against an isolated Rex server.

use std::sync::Arc;
use std::time::{Duration, Instant};

use rex_proto::control::Control;
use rex_proto::testing::TestServer;
use rexterm::rex::{BlockRef, RexSource, attach};
use rexterm::terminal::input::KeyInput;
use rexterm::terminal::{FrameHandle, FrameSink, FrameSource, StartOptions, TerminalInput, TerminalSnapshot, Viewport};
use serde_json::json;

fn session(ctl: &Control, command: Option<&[&str]>) -> (String, String) {
    let mut block = json!({"flavor": "com.superlogical.terminal.shell"});
    if let Some(c) = command {
        block["options"] = json!({"command": c});
    }
    let created = ctl.call("session.create", json!({"initial_windows": [{"layout": {"block": block}}]})).unwrap();
    let sid = created["session_id"].as_str().unwrap().to_string();
    let att = ctl.call("session.attach", json!({"session_id": sid})).unwrap();
    let view = &att["view"];
    let active = view["active_window_id"].as_str().unwrap();
    let win = view["windows"].as_array().unwrap().iter().find(|w| w["window_id"] == active).unwrap();
    (sid, win["focused_block_id"].as_str().unwrap().to_string())
}

struct Running {
    sink: FrameSink,
    handle: Box<dyn FrameHandle>,
}

fn start(srv: &TestServer, ctl: &Arc<Control>, sid: &str, bid: &str) -> Running {
    let block = BlockRef { endpoint: srv.endpoint.clone(), control: ctl.clone(), session_id: sid.into(), block_id: bid.into() };
    let attached = attach(&block).unwrap();
    let (source, _events) = RexSource::new(block, attached);
    let (sink, _wake) = FrameSink::new();
    let handle = Box::new(source.engine())
        .start(sink.clone(), StartOptions::default().with_viewport(Viewport::new(80, 24, 8, 16).unwrap()))
        .unwrap();
    Running { sink, handle }
}

fn wait_for(r: &Running, what: &str, done: impl Fn(&TerminalSnapshot) -> bool) -> Arc<TerminalSnapshot> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last = None;
    while Instant::now() < deadline {
        if let Some(s) = r.sink.take() {
            r.handle.request_frame();
            if done(&s) {
                return s;
            }
            last = Some(s);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}; last frame:\n{}", last.map_or_else(String::new, |s| s.frame.text()));
}

fn key(r: &Running, k: &str, text: Option<&str>) {
    let mut ki = KeyInput::new(k);
    if let Some(t) = text {
        ki = ki.with_text(t);
    }
    r.handle.input(TerminalInput::Key(ki)).unwrap();
}

#[test]
fn typing_reaches_program() {
    let Some(srv) = TestServer::start() else { return };
    let ctl = Control::connect(srv.endpoint.clone(), "rexa-desktop-test").unwrap();
    let (sid, bid) = session(&ctl, None);
    let r = start(&srv, &ctl, &sid, &bid);
    std::thread::sleep(Duration::from_millis(500));
    for c in ["e", "c", "h", "o"] {
        key(&r, c, Some(c));
    }
    key(&r, "space", Some(" "));
    // Quote-split so the echoed command line never matches.
    for c in ["'", "O", "'", "K"] {
        key(&r, c, Some(c));
    }
    key(&r, "enter", None);
    wait_for(&r, "OK", |s| s.frame.text().lines().any(|l| l.trim() == "OK"));
}

#[test]
fn ime_text_goes_to_write() {
    let Some(srv) = TestServer::start() else { return };
    let ctl = Control::connect(srv.endpoint.clone(), "rexa-desktop-test").unwrap();
    let (sid, bid) = session(&ctl, Some(&["cat"]));
    let r = start(&srv, &ctl, &sid, &bid);
    r.handle.input(TerminalInput::Text("中文测试".into())).unwrap();
    key(&r, "enter", None);
    wait_for(&r, "中文测试 twice (typed and echoed by cat)", |s| s.frame.text().matches("中文测试").count() >= 2);
    let cap = srv.rex(&["capture", "-s", &sid]);
    assert!(cap.contains("中文测试"), "{cap}");
}

#[test]
fn paste_keeps_text() {
    let Some(srv) = TestServer::start() else { return };
    let ctl = Control::connect(srv.endpoint.clone(), "rexa-desktop-test").unwrap();
    let (sid, bid) = session(&ctl, Some(&["cat", "-v"]));
    let r = start(&srv, &ctl, &sid, &bid);
    r.handle.input(TerminalInput::Paste("第一行\n第二行".into())).unwrap();
    wait_for(&r, "both pasted lines", |s| {
        let t = s.frame.text();
        t.contains("第一行") && t.contains("第二行")
    });
}

#[test]
fn server_resize_updates_grid() {
    let Some(srv) = TestServer::start() else { return };
    let ctl = Control::connect(srv.endpoint.clone(), "rexa-desktop-test").unwrap();
    let (sid, bid) = session(&ctl, None);
    let r = start(&srv, &ctl, &sid, &bid);
    let other = Control::connect(srv.endpoint.clone(), "other-client").unwrap();
    other.call("session.attach", json!({"session_id": sid})).unwrap();
    other.call("session.own_block", json!({"session_id": sid, "block_ids": [bid], "mode": "take"})).unwrap();
    other
        .call("com.superlogical.terminal.resize", json!({"session_id": sid, "block_id": bid, "args": {"columns": 100, "rows": 30}}))
        .unwrap();
    wait_for(&r, "a 100x30 grid", |s| s.frame.viewport.columns() == 100 && s.frame.viewport.rows() == 30);
}
