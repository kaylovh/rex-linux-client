//! An isolated Rex server for tests: its own socket and state directory, never the user's.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use crate::transport::Endpoint;

pub struct TestServer {
    pub endpoint: Endpoint,
    pub socket: PathBuf,
    state_dir: PathBuf,
    child: Child,
}

static NEXT: AtomicU32 = AtomicU32::new(0);

impl TestServer {
    /// Starts a server, or None when no `rex` is on PATH (the caller's test then passes
    /// after saying so).
    pub fn start() -> Option<TestServer> {
        if Command::new("rex").arg("version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_err() {
            eprintln!("rex is not on PATH: skipping a live test");
            return None;
        }
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let run = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let socket = run.join(format!("rexa-desktop-test-{pid}-{n}.sock"));
        let state_dir = std::env::temp_dir().join(format!("rexa-desktop-test-{pid}-{n}"));
        let _ = std::fs::remove_file(&socket);
        std::fs::create_dir_all(&state_dir).expect("state dir");
        let child = Command::new("rex")
            .args(["server", "run", "--listen"])
            .arg(format!("unix://{}", socket.display()))
            .arg("--state-dir")
            .arg(&state_dir)
            .arg("--restore=false")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start rex server");
        let start = Instant::now();
        while std::os::unix::net::UnixStream::connect(&socket).is_err() {
            assert!(start.elapsed() < Duration::from_secs(10), "test server did not come up");
            std::thread::sleep(Duration::from_millis(50));
        }
        Some(TestServer { endpoint: Endpoint::Unix(socket.clone()), socket, state_dir, child })
    }

    /// The address for `rex -S`.
    pub fn address(&self) -> String {
        format!("unix://{}", self.socket.display())
    }

    /// Runs the rex CLI against this server and returns its stdout.
    pub fn rex(&self, args: &[&str]) -> String {
        let out = Command::new("rex").arg("-S").arg(self.address()).args(args).output().expect("run rex");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = Command::new("rex")
            .args(["-S", &self.address(), "server", "stop"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket);
        let _ = std::fs::remove_dir_all(&self.state_dir);
    }
}
