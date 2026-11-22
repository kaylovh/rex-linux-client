//! Where a Rex server is and how to reach it: a unix socket, or a command (usually ssh)
//! whose stdin/stdout is bridged to the server by `rex __internal dial-stdio`.

use std::io::{self, Read};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::ws::Stream;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    Unix(PathBuf),
    /// argv of a stdio bridge command
    Command(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError(pub String);

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "bad server address: {}", self.0)
    }
}
impl std::error::Error for ParseError {}

/// Put in front of a remote command so a non-interactive ssh shell finds rex.
pub const REMOTE_PATH: &str = "PATH=\"$PATH:$HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin:/Applications/Rex.app/Contents/MacOS:/Applications/Rex.app/Contents/Resources\"; export PATH; ";

/// it's -> 'it'\''s', for the remote shell
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Splits a command line into words, honouring '...' and "..." quotes and \ escapes.
fn split_words(s: &str) -> Result<Vec<String>, ParseError> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => cur.push(c),
                        None => return Err(ParseError(s.into())),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => cur.extend(chars.next()),
                        Some(c) => cur.push(c),
                        None => return Err(ParseError(s.into())),
                    }
                }
            }
            '\\' => {
                in_word = true;
                cur.extend(chars.next());
            }
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                cur.push(c);
            }
        }
    }
    if in_word {
        words.push(cur);
    }
    Ok(words)
}

fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"))
}

impl Endpoint {
    /// unix:///path, /path, ssh://[user@]host[:port][#hop], cmd:<argv words>, or anything
    /// else (a host label, host:port), which the rex CLI reaches for us.
    pub fn parse(s: &str) -> Result<Endpoint, ParseError> {
        if let Some(p) = s.strip_prefix("unix://") {
            return Ok(Endpoint::Unix(p.into()));
        }
        if s.starts_with('/') {
            return Ok(Endpoint::Unix(s.into()));
        }
        if let Some(c) = s.strip_prefix("cmd:") {
            let argv = split_words(c)?;
            if argv.is_empty() {
                return Err(ParseError(s.into()));
            }
            return Ok(Endpoint::Command(argv));
        }
        if let Some(rest) = s.strip_prefix("ssh://") {
            let mut rest = rest.trim_end_matches('/');
            let mut hop = "";
            if let Some(i) = rest.find('#') {
                hop = &rest[i + 1..];
                rest = rest[..i].trim_end_matches('/');
            }
            if rest.is_empty() {
                return Err(ParseError(s.into()));
            }
            let control_path = runtime_dir().join("rexa-desktop-%C");
            let mut argv: Vec<String> = [
                "ssh", "-T", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", "-o", "ServerAliveInterval=15",
                "-o", "ControlMaster=auto", "-o",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect();
            argv.push(format!("ControlPath={}", control_path.display()));
            argv.extend(["-o".to_string(), "ControlPersist=60".to_string()]);
            if let Some(i) = rest.rfind(':') {
                let port = &rest[i + 1..];
                if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) {
                    argv.extend(["-p".to_string(), port.to_string()]);
                    rest = &rest[..i];
                }
            }
            argv.push(rest.to_string());
            argv.push(if hop.is_empty() {
                format!("{REMOTE_PATH}exec rex __internal dial-stdio")
            } else {
                format!("{REMOTE_PATH}exec rex --server {} __internal dial-stdio", shell_quote(hop))
            });
            return Ok(Endpoint::Command(argv));
        }
        if s.is_empty() {
            return Err(ParseError(s.into()));
        }
        Ok(Endpoint::Command(
            ["rex", "--server", s, "__internal", "dial-stdio"].iter().map(|s| s.to_string()).collect(),
        ))
    }

    /// The local server: `$REX_SERVER` if set, else the platform's default socket.
    pub fn default_local() -> Endpoint {
        if let Ok(s) = std::env::var("REX_SERVER")
            && let Ok(ep) = Endpoint::parse(&s)
        {
            return ep;
        }
        if cfg!(target_os = "macos") {
            let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
            return Endpoint::Unix(home.join("Library/Application Support/rex/server.sock"));
        }
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() };
        Endpoint::Unix(PathBuf::from(format!("/run/user/{uid}/rex/server.sock")))
    }

    /// Opens a byte stream. A command that writes nothing within `timeout` is killed, so a
    /// hung ssh can't block forever; one that exits early fails the first read with the last
    /// line it wrote to stderr.
    pub fn connect(&self, timeout: Duration) -> io::Result<Stream> {
        match self {
            Endpoint::Unix(path) => {
                let s = UnixStream::connect(path)?;
                let r = s.try_clone()?;
                let c = s.try_clone()?;
                Ok(Stream {
                    reader: Box::new(r),
                    writer: Box::new(s),
                    closer: Arc::new(move || {
                        let _ = c.shutdown(std::net::Shutdown::Both);
                    }),
                })
            }
            Endpoint::Command(argv) => {
                let mut child = Command::new(&argv[0])
                    .args(&argv[1..])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", argv[0])))?;
                let stdout = child.stdout.take().unwrap();
                let stdin = child.stdin.take().unwrap();
                let stderr = child.stderr.take().unwrap();
                let last_err = Arc::new(Mutex::new(String::new()));
                {
                    let last_err = last_err.clone();
                    std::thread::spawn(move || {
                        let mut buf = String::new();
                        let _ = io::BufReader::new(stderr).read_to_string(&mut buf);
                        if let Some(l) = buf.lines().rev().find(|l| !l.trim().is_empty()) {
                            *last_err.lock().unwrap() = l.trim().to_string();
                        }
                    });
                }
                let child = Arc::new(Mutex::new(child));
                let got_data = Arc::new(AtomicBool::new(false));
                {
                    let (child, got_data) = (child.clone(), got_data.clone());
                    std::thread::spawn(move || {
                        std::thread::sleep(timeout);
                        if !got_data.load(Ordering::Acquire) {
                            let _ = child.lock().unwrap().kill();
                        }
                    });
                }
                let closer_child = child.clone();
                Ok(Stream {
                    reader: Box::new(ChildReader { inner: stdout, child, got_data, last_err, name: argv[0].clone() }),
                    writer: Box::new(stdin),
                    closer: Arc::new(move || {
                        let mut c = closer_child.lock().unwrap();
                        let _ = c.kill();
                        let _ = c.try_wait();
                    }),
                })
            }
        }
    }
}

/// A bridge command's stdout. EOF before any byte becomes an error that says why.
struct ChildReader {
    inner: std::process::ChildStdout,
    child: Arc<Mutex<Child>>,
    got_data: Arc<AtomicBool>,
    last_err: Arc<Mutex<String>>,
    name: String,
}

impl Read for ChildReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n > 0 {
            self.got_data.store(true, Ordering::Release);
            return Ok(n);
        }
        if !self.got_data.load(Ordering::Acquire) {
            let status = self.child.lock().unwrap().wait().ok();
            // stderr is read on its own thread; give it a moment to land.
            std::thread::sleep(Duration::from_millis(50));
            let why = self.last_err.lock().unwrap().clone();
            let status = status.map(|s| s.to_string()).unwrap_or_default();
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                if why.is_empty() { format!("{} {status}", self.name) } else { format!("{}: {why}", self.name) },
            ));
        }
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn argv(ep: &Endpoint) -> &[String] {
        match ep {
            Endpoint::Command(a) => a,
            Endpoint::Unix(_) => panic!("not a command"),
        }
    }

    #[test]
    fn parse_ssh_with_port_user_hop() {
        let ep = Endpoint::parse("ssh://me@box:2222").unwrap();
        let a = argv(&ep);
        let p = a.iter().position(|x| x == "-p").unwrap();
        assert_eq!(a[p + 1], "2222");
        assert_eq!(a[p + 2], "me@box");
        assert!(a.last().unwrap().ends_with("exec rex __internal dial-stdio"));

        let hop = Endpoint::parse("ssh://me@mac:22/#it's P1").unwrap();
        let last = argv(&hop).last().unwrap();
        assert!(last.contains(r"exec rex --server 'it'\''s P1' __internal dial-stdio"), "{last}");
    }

    #[test]
    fn ssh_args_include_control_master() {
        let ep = Endpoint::parse("ssh://box").unwrap();
        let a = argv(&ep);
        assert!(a.iter().any(|x| x == "ControlMaster=auto"));
        assert!(a.iter().any(|x| x == "ControlPersist=60"));
        assert!(a.iter().any(|x| x.starts_with("ControlPath=") && x.ends_with("/rexa-desktop-%C")));
        assert_eq!(a[0], "ssh");
    }

    #[test]
    fn parse_unix_forms() {
        assert_eq!(Endpoint::parse("unix:///a/b").unwrap(), Endpoint::Unix("/a/b".into()));
        assert_eq!(Endpoint::parse("/a/b").unwrap(), Endpoint::Unix("/a/b".into()));
        assert!(Endpoint::parse("").is_err());
    }

    #[test]
    fn host_label_goes_through_rex_cli() {
        let ep = Endpoint::parse("devbox").unwrap();
        assert_eq!(argv(&ep), ["rex", "--server", "devbox", "__internal", "dial-stdio"]);
    }

    #[test]
    fn command_endpoint_that_exits_fails_fast() {
        let ep = Endpoint::parse("cmd:sh -c 'echo nope >&2; exit 3'").unwrap();
        let start = Instant::now();
        let err = match ep.connect(Duration::from_secs(10)) {
            Err(e) => e,
            Ok(stream) => match crate::ws::connect(stream, "/api/control", "rex.control.v1") {
                Err(e) => e,
                Ok(_) => panic!("connected to a command that exited"),
            },
        };
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(err.to_string().contains("nope"), "{err}");
    }

    #[test]
    fn silent_command_is_killed_at_timeout() {
        let ep = Endpoint::parse("cmd:sleep 30").unwrap();
        let start = Instant::now();
        let stream = ep.connect(Duration::from_millis(300)).unwrap();
        assert!(crate::ws::connect(stream, "/api/control", "rex.control.v1").is_err());
        assert!(start.elapsed() < Duration::from_secs(3));
    }
}
