//! Local changes for rexa-desktop: an engine that starts from a snapshot, takes its grid size
//! from its source, and lets the source take input before it is encoded.

#![cfg(feature = "ghostty")]

use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gpui_cn::terminal::input::KeyInput;
use gpui_cn::terminal::{
    ByteHandle, ByteSink, ByteSource, Engine, EngineOptions, ExitStatus, FrameHandle, FrameSink, FrameSource,
    InputRoute, StartOptions, TerminalInput, TerminalSnapshot, Viewport,
};

#[derive(Default)]
struct Seen {
    sink: Option<ByteSink>,
    writes: Vec<Vec<u8>>,
    resizes: Vec<Viewport>,
    routed: Vec<TerminalInput>,
}

struct TestSource {
    seen: Arc<Mutex<Seen>>,
    handle_keys: bool,
}

struct TestHandle {
    seen: Arc<Mutex<Seen>>,
    handle_keys: bool,
}

impl ByteSource for TestSource {
    fn start(self: Box<Self>, sink: ByteSink, _viewport: Viewport) -> io::Result<Box<dyn ByteHandle>> {
        self.seen.lock().unwrap().sink = Some(sink);
        Ok(Box::new(TestHandle { seen: self.seen, handle_keys: self.handle_keys }))
    }
}

impl ByteHandle for TestHandle {
    fn write(&self, bytes: &[u8]) -> io::Result<usize> {
        self.seen.lock().unwrap().writes.push(bytes.to_vec());
        Ok(bytes.len())
    }
    fn resize(&self, viewport: Viewport) -> io::Result<()> {
        self.seen.lock().unwrap().resizes.push(viewport);
        Ok(())
    }
    fn try_wait(&self) -> io::Result<Option<ExitStatus>> {
        Ok(None)
    }
    fn close(&self) {}
    fn route_input(&self, input: &TerminalInput) -> InputRoute {
        if self.handle_keys && matches!(input, TerminalInput::Key(_)) {
            self.seen.lock().unwrap().routed.push(input.clone());
            InputRoute::Handled
        } else {
            InputRoute::Encode
        }
    }
}

fn wait_for(sink: &FrameSink, handle: &dyn FrameHandle, done: impl Fn(&TerminalSnapshot) -> bool) -> Arc<TerminalSnapshot> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last = None;
    while Instant::now() < deadline {
        if let Some(s) = sink.take() {
            handle.request_frame();
            if done(&s) {
                return s;
            }
            last = Some(s);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("timed out; last frame:\n{}", last.map_or_else(String::new, |s| format!("{:?}\n{}", s.frame.viewport, s.frame.text())));
}

fn start(options: EngineOptions, handle_keys: bool, viewport: Viewport) -> (Arc<Mutex<Seen>>, FrameSink, Box<dyn FrameHandle>) {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let (sink, _wake) = FrameSink::new();
    let handle = Box::new(Engine::new(TestSource { seen: seen.clone(), handle_keys }, options))
        .start(sink.clone(), StartOptions::default().with_viewport(viewport))
        .unwrap();
    (seen, sink, handle)
}

fn byte_sink(seen: &Arc<Mutex<Seen>>) -> ByteSink {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(s) = seen.lock().unwrap().sink.clone() {
            return s;
        }
        assert!(Instant::now() < deadline, "source never started");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn engine_starts_from_snapshot() {
    let mut t = ghostty_vt::Terminal::new(ghostty_vt::TerminalOptions { cols: 20, rows: 3, ..Default::default() }).unwrap();
    t.vt_write(b"hi");
    let snap = ghostty_vt::snapshot::encode(&t).unwrap().to_vec();
    let (_seen, sink, handle) = start(
        EngineOptions::default().with_snapshot(snap).with_source_sized(true),
        false,
        Viewport::new(80, 24, 8, 16).unwrap(),
    );
    let shown = wait_for(&sink, handle.as_ref(), |s| s.frame.text().starts_with("hi"));
    assert_eq!(shown.frame.viewport.columns(), 20);
    assert_eq!(shown.frame.viewport.rows(), 3);
}

#[test]
fn grid_follows_source_size() {
    let (seen, sink, handle) = start(EngineOptions::default().with_source_sized(true), false, Viewport::new(100, 40, 8, 16).unwrap());
    let bs = byte_sink(&seen);
    bs.set_grid(80, 24);
    let s = wait_for(&sink, handle.as_ref(), |s| s.frame.viewport.columns() == 80);
    assert_eq!(s.frame.viewport.rows(), 24);

    let smaller = Viewport::new(50, 10, 8, 16).unwrap();
    handle.resize(smaller);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !seen.lock().unwrap().resizes.contains(&smaller) {
        assert!(Instant::now() < deadline, "source never heard of the new size");
        std::thread::sleep(Duration::from_millis(5));
    }
    // Output still lands in the 80x24 grid the source set.
    bs.write(b"after");
    let s = wait_for(&sink, handle.as_ref(), |s| s.frame.text().contains("after"));
    assert_eq!((s.frame.viewport.columns(), s.frame.viewport.rows()), (80, 24));
}

#[test]
fn grid_change_keeps_its_place_in_the_stream() {
    let (seen, sink, handle) = start(EngineOptions::default().with_source_sized(true), false, Viewport::new(80, 24, 8, 16).unwrap());
    let bs = byte_sink(&seen);
    // 30 columns of text, then a narrower grid: the text was written at 80 columns.
    bs.write(&[b'x'; 30]);
    bs.set_grid(10, 5);
    let s = wait_for(&sink, handle.as_ref(), |s| s.frame.viewport.columns() == 10);
    assert!(s.frame.text().contains("xxxxxxxxxx"));
}

#[test]
fn route_input_handled_skips_encoding() {
    let (seen, _sink, handle) = start(EngineOptions::default(), true, Viewport::new(80, 24, 8, 16).unwrap());
    byte_sink(&seen);
    handle.input(TerminalInput::Key(KeyInput::new("a").with_text("a"))).unwrap();
    handle.input(TerminalInput::Paste("p".into())).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while seen.lock().unwrap().writes.is_empty() {
        assert!(Instant::now() < deadline, "paste never written");
        std::thread::sleep(Duration::from_millis(5));
    }
    let seen = seen.lock().unwrap();
    assert_eq!(seen.routed.len(), 1);
    // Only the paste (bracketed or not) reached write; the key did not.
    let all: Vec<u8> = seen.writes.concat();
    assert!(!all.contains(&b'a'), "{all:?}");
    assert!(all.contains(&b'p'));
}
