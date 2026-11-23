//! Files on an ssh machine over SFTP, through a child process that speaks it on its
//! stdin/stdout: `ssh … -s host sftp` over the machine's shared connection, or `sftp-server`
//! itself in tests. Calls are one request and its answer at a time.

pub mod wire;

use std::io::{BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, UNIX_EPOCH};

use wire::{Attrs, In, Out};

use crate::{FileEntry, FileSystem, FsError, Kind, Listing, join};

/// The most one READ asks for.
const CHUNK: u32 = 32 * 1024;
/// A packet bigger than this is a broken stream, not a reply.
const MAX_PACKET: usize = 1 << 20;

struct Conn {
    child: Child,
    to: ChildStdin,
    from: BufReader<ChildStdout>,
    next_id: u32,
    posix_rename: bool,
}

/// What went wrong talking to the server: the stream (connection gone) or the request.
enum Fail {
    Gone(String),
    Status(u32, String),
    Bad(String),
}

impl Conn {
    fn open(argv: &[String]) -> Result<Conn, FsError> {
        let (cmd, args) = argv.split_first().ok_or_else(|| FsError::Other("no SFTP command".into()))?;
        let mut child = Command::new(cmd)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| FsError::Gone(format!("{cmd}: {e}")))?;
        let to = child.stdin.take().expect("piped");
        let from = BufReader::new(child.stdout.take().expect("piped"));
        let mut c = Conn { child, to, from, next_id: 1, posix_rename: false };
        c.send(&Out::new(wire::INIT).u32(3).finish()).map_err(gone)?;
        let reply = c.recv().map_err(gone)?;
        let mut r = In::new(&reply);
        if r.u8() != Ok(wire::VERSION) {
            return Err(FsError::Gone("not an SFTP server".into()));
        }
        let _version = r.u32();
        while !r.is_empty() {
            let (Ok(name), Ok(_)) = (r.string(), r.bytes()) else { break };
            if name == "posix-rename@openssh.com" {
                c.posix_rename = true;
            }
        }
        Ok(c)
    }

    fn send(&mut self, packet: &[u8]) -> Result<(), Fail> {
        self.to.write_all(packet).and_then(|_| self.to.flush()).map_err(|e| Fail::Gone(e.to_string()))
    }

    fn recv(&mut self) -> Result<Vec<u8>, Fail> {
        let mut len = [0u8; 4];
        self.from.read_exact(&mut len).map_err(|e| Fail::Gone(e.to_string()))?;
        let len = u32::from_be_bytes(len) as usize;
        if len == 0 || len > MAX_PACKET {
            return Err(Fail::Gone(format!("bad packet length {len}")));
        }
        let mut body = vec![0u8; len];
        self.from.read_exact(&mut body).map_err(|e| Fail::Gone(e.to_string()))?;
        Ok(body)
    }

    fn id(&mut self) -> u32 {
        self.next_id = self.next_id.wrapping_add(1);
        self.next_id
    }

    /// Sends a request (`build` gets the id) and returns the reply's type and the rest.
    fn call(&mut self, kind: u8, build: impl FnOnce(Out) -> Out) -> Result<(u8, Vec<u8>), Fail> {
        let id = self.id();
        self.send(&build(Out::new(kind).u32(id)).finish())?;
        loop {
            let body = self.recv()?;
            let mut r = In::new(&body);
            let (Ok(t), Ok(rid)) = (r.u8(), r.u32()) else { return Err(Fail::Gone("short reply".into())) };
            if rid != id {
                continue;
            }
            return Ok((t, body[5..].to_vec()));
        }
    }

    fn status_of(t: u8, body: &[u8]) -> Result<(), Fail> {
        if t != wire::STATUS {
            return Err(Fail::Bad(format!("unexpected reply {t}")));
        }
        let mut r = In::new(body);
        let code = r.u32().map_err(|_| Fail::Bad("short status".into()))?;
        let msg = r.string().unwrap_or_default();
        if code == wire::FX_OK { Ok(()) } else { Err(Fail::Status(code, msg)) }
    }

    fn simple(&mut self, kind: u8, build: impl FnOnce(Out) -> Out) -> Result<(), Fail> {
        let (t, body) = self.call(kind, build)?;
        Self::status_of(t, &body)
    }

    fn handle(&mut self, kind: u8, build: impl FnOnce(Out) -> Out) -> Result<Vec<u8>, Fail> {
        let (t, body) = self.call(kind, build)?;
        if t == wire::HANDLE {
            return In::new(&body).bytes().map(<[u8]>::to_vec).map_err(|_| Fail::Bad("short handle".into()));
        }
        Self::status_of(t, &body)?;
        Err(Fail::Bad("no handle".into()))
    }

    fn close(&mut self, handle: &[u8]) {
        let _ = self.simple(wire::CLOSE, |o| o.bytes(handle));
    }

    fn attrs(&mut self, kind: u8, path: &str) -> Result<Attrs, Fail> {
        let (t, body) = self.call(kind, |o| o.str(path))?;
        if t == wire::ATTRS {
            return In::new(&body).attrs().map_err(|_| Fail::Bad("short attrs".into()));
        }
        Self::status_of(t, &body)?;
        Err(Fail::Bad("no attrs".into()))
    }

    fn realpath(&mut self, path: &str) -> Result<String, Fail> {
        let (t, body) = self.call(wire::REALPATH, |o| o.str(path))?;
        if t == wire::NAME {
            let mut r = In::new(&body);
            let _count = r.u32();
            return r.string().map_err(|_| Fail::Bad("short name".into()));
        }
        Self::status_of(t, &body)?;
        Err(Fail::Bad("no name".into()))
    }

    /// The names and attributes in a directory, without . and ..
    fn read_dir(&mut self, path: &str) -> Result<Vec<(String, Attrs)>, Fail> {
        let h = self.handle(wire::OPENDIR, |o| o.str(path))?;
        let mut out = Vec::new();
        let result = loop {
            match self.call(wire::READDIR, |o| o.bytes(&h)) {
                Ok((wire::NAME, body)) => {
                    let mut r = In::new(&body);
                    let Ok(count) = r.u32() else { break Err(Fail::Bad("short name".into())) };
                    for _ in 0..count {
                        let (Ok(name), Ok(_long), Ok(a)) = (r.string(), r.bytes(), r.attrs()) else {
                            return Err(Fail::Bad("short name".into()));
                        };
                        if name != "." && name != ".." {
                            out.push((name, a));
                        }
                    }
                }
                Ok((t, body)) => match Self::status_of(t, &body) {
                    Err(Fail::Status(wire::FX_EOF, _)) => break Ok(()),
                    Err(e) => break Err(e),
                    Ok(()) => break Ok(()),
                },
                Err(e) => break Err(e),
            }
        };
        self.close(&h);
        result.map(|_| out)
    }
}

fn gone(f: Fail) -> FsError {
    match f {
        Fail::Gone(why) => FsError::Gone(why),
        other => to_fs(other),
    }
}

fn to_fs(f: Fail) -> FsError {
    match f {
        Fail::Gone(why) => FsError::Gone(why),
        Fail::Status(wire::FX_NO_SUCH_FILE, _) => FsError::NotFound,
        Fail::Status(wire::FX_PERMISSION_DENIED, _) => FsError::Denied,
        Fail::Status(_, msg) => FsError::Other(msg),
        Fail::Bad(why) => FsError::Other(why),
    }
}

const S_IFMT: u32 = 0o170000;
const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;
const S_IFLNK: u32 = 0o120000;

fn kind_of(a: &Attrs) -> Kind {
    match a.permissions.map(|p| p & S_IFMT) {
        Some(S_IFDIR) => Kind::Dir,
        Some(S_IFREG) => Kind::File,
        Some(S_IFLNK) => Kind::Symlink { to_dir: false },
        _ => Kind::Other,
    }
}

fn entry(name: String, a: &Attrs) -> FileEntry {
    FileEntry {
        name,
        kind: kind_of(a),
        size: a.size,
        modified: a.times.map(|(_, m)| UNIX_EPOCH + Duration::from_secs(u64::from(m))),
        mode: a.permissions,
    }
}

fn name_of(path: &str) -> String {
    path.trim_end_matches('/').rsplit('/').next().unwrap_or(path).to_string()
}

pub struct SftpFs {
    argv: Vec<String>,
    conn: Mutex<Option<Conn>>,
}

impl SftpFs {
    /// Starts the child and says hello; the connection is made again after it dies.
    pub fn spawn(argv: Vec<String>) -> Result<SftpFs, FsError> {
        let conn = Conn::open(&argv)?;
        Ok(SftpFs { argv, conn: Mutex::new(Some(conn)) })
    }

    /// Runs `f` on the connection, opening one if there is none; a lost stream drops it
    /// (the next call opens a new one) and reports Gone.
    fn with<T>(&self, f: impl FnOnce(&mut Conn) -> Result<T, Fail>) -> Result<T, FsError> {
        let mut guard = self.conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.is_none() {
            *guard = Some(Conn::open(&self.argv)?);
        }
        let conn = guard.as_mut().expect("just set");
        match f(conn) {
            Ok(v) => Ok(v),
            Err(Fail::Gone(why)) => {
                if let Some(mut c) = guard.take() {
                    let _ = c.child.kill();
                    let _ = c.child.wait();
                }
                Err(FsError::Gone(why))
            }
            Err(e) => Err(to_fs(e)),
        }
    }

    #[cfg(test)]
    fn kill_for_test(&self) {
        if let Some(c) = self.conn.lock().unwrap().as_mut() {
            let _ = c.child.kill();
            let _ = c.child.wait();
        }
    }
}

impl Drop for SftpFs {
    fn drop(&mut self) {
        if let Some(mut c) = self.conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take() {
            let _ = c.child.kill();
            let _ = c.child.wait();
        }
    }
}

/// Whether something is at `path` (not following a link).
fn exists(c: &mut Conn, path: &str) -> Result<bool, Fail> {
    match c.attrs(wire::LSTAT, path) {
        Ok(_) => Ok(true),
        Err(Fail::Status(wire::FX_NO_SUCH_FILE, _)) => Ok(false),
        Err(e) => Err(e),
    }
}

fn remove_tree(c: &mut Conn, path: &str) -> Result<(), Fail> {
    let a = c.attrs(wire::LSTAT, path)?;
    if kind_of(&a) == Kind::Dir {
        for (name, a) in c.read_dir(path)? {
            let p = join(path, &name);
            if kind_of(&a) == Kind::Dir {
                remove_tree(c, &p)?;
            } else {
                c.simple(wire::REMOVE, |o| o.str(&p))?;
            }
        }
        c.simple(wire::RMDIR, |o| o.str(path))
    } else {
        c.simple(wire::REMOVE, |o| o.str(path))
    }
}

/// A status that means "something is already there" on servers that only say FAILURE.
fn exists_or(c: &mut Conn, path: &str, e: Fail) -> Fail {
    match e {
        Fail::Status(code, msg) if code != wire::FX_NO_SUCH_FILE && code != wire::FX_PERMISSION_DENIED => {
            if exists(c, path).unwrap_or(false) { Fail::Status(u32::MAX, "exists".into()) } else { Fail::Status(code, msg) }
        }
        other => other,
    }
}

impl FileSystem for SftpFs {
    fn home(&self) -> Result<String, FsError> {
        self.with(|c| c.realpath("."))
    }

    fn list(&self, path: &str) -> Result<Listing, FsError> {
        self.with(|c| {
            let abs = c.realpath(path)?;
            let a = c.attrs(wire::STAT, &abs)?;
            if kind_of(&a) != Kind::Dir {
                return Err(Fail::Status(u32::MAX - 1, "not a directory".into()));
            }
            let mut entries = Vec::new();
            for (name, a) in c.read_dir(&abs)? {
                let mut e = entry(name, &a);
                if let Kind::Symlink { .. } = e.kind {
                    let to_dir = c.attrs(wire::STAT, &join(&abs, &e.name)).is_ok_and(|t| kind_of(&t) == Kind::Dir);
                    e.kind = Kind::Symlink { to_dir };
                }
                entries.push(e);
            }
            Ok(Listing { path: abs, entries, truncated: false })
        })
        .map_err(not_dir)
    }

    fn stat(&self, path: &str) -> Result<FileEntry, FsError> {
        self.with(|c| {
            let a = c.attrs(wire::LSTAT, path)?;
            let mut e = entry(name_of(path), &a);
            if let Kind::Symlink { .. } = e.kind {
                let to_dir = c.attrs(wire::STAT, path).is_ok_and(|t| kind_of(&t) == Kind::Dir);
                e.kind = Kind::Symlink { to_dir };
            }
            Ok(e)
        })
    }

    fn read(&self, path: &str, max: u64) -> Result<Vec<u8>, FsError> {
        self.with(|c| {
            let h = c.handle(wire::OPEN, |o| o.str(path).u32(wire::READ_FLAG).attrs(&Attrs::default()))?;
            let mut out = Vec::new();
            let result = loop {
                let left = max.saturating_sub(out.len() as u64);
                if left == 0 {
                    break Ok(());
                }
                let n = left.min(u64::from(CHUNK)) as u32;
                let offset = out.len() as u64;
                match c.call(wire::READ, |o| o.bytes(&h).u64(offset).u32(n)) {
                    Ok((wire::DATA, body)) => match In::new(&body).bytes() {
                        Ok(d) if !d.is_empty() => out.extend_from_slice(d),
                        _ => break Ok(()),
                    },
                    Ok((t, body)) => match Conn::status_of(t, &body) {
                        Err(Fail::Status(wire::FX_EOF, _)) | Ok(()) => break Ok(()),
                        Err(e) => break Err(e),
                    },
                    Err(e) => break Err(e),
                }
            };
            c.close(&h);
            result.map(|_| out)
        })
    }

    fn create_file(&self, path: &str) -> Result<(), FsError> {
        self.with(|c| {
            let flags = wire::WRITE_FLAG | wire::CREAT | wire::EXCL;
            match c.handle(wire::OPEN, |o| o.str(path).u32(flags).attrs(&Attrs::default())) {
                Ok(h) => {
                    c.close(&h);
                    Ok(())
                }
                Err(e) => Err(exists_or(c, path, e)),
            }
        })
        .map_err(exists_err)
    }

    fn create_dir(&self, path: &str) -> Result<(), FsError> {
        self.with(|c| c.simple(wire::MKDIR, |o| o.str(path).attrs(&Attrs::default())).map_err(|e| exists_or(c, path, e)))
            .map_err(exists_err)
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), FsError> {
        self.with(|c| {
            if exists(c, to)? {
                return Err(Fail::Status(u32::MAX, "exists".into()));
            }
            if c.posix_rename {
                c.simple(wire::EXTENDED, |o| o.str("posix-rename@openssh.com").str(from).str(to))
            } else {
                c.simple(wire::RENAME, |o| o.str(from).str(to))
            }
        })
        .map_err(exists_err)
    }

    fn remove(&self, path: &str) -> Result<(), FsError> {
        self.with(|c| remove_tree(c, path))
    }

    fn writable(&self) -> Result<(), String> {
        Ok(())
    }
}

/// Our own marker statuses: u32::MAX "exists", u32::MAX - 1 "not a directory".
fn exists_err(e: FsError) -> FsError {
    if e == FsError::Other("exists".into()) { FsError::Exists } else { e }
}

fn not_dir(e: FsError) -> FsError {
    if e == FsError::Other("not a directory".into()) { FsError::NotDir } else { e }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERVER: &str = "/usr/libexec/openssh/sftp-server";

    fn server_argv(root: &str) -> Option<Vec<String>> {
        if !std::path::Path::new(SERVER).exists() {
            eprintln!("{SERVER} is missing: skipping an SFTP test");
            return None;
        }
        Some(vec![SERVER.into(), "-d".into(), root.into()])
    }

    fn temp_root(name: &str) -> String {
        let d = std::env::temp_dir().join(format!("rexa-sftp-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.to_string_lossy().into_owned()
    }

    #[test]
    fn sftp_conformance() {
        let root = temp_root("conf");
        let Some(argv) = server_argv(&root) else { return };
        let fs = SftpFs::spawn(argv).unwrap();
        crate::conformance::run(
            &fs,
            &root,
            &|p, b| std::fs::write(p, b).unwrap(),
            &|target, p| std::os::unix::fs::symlink(target, p).unwrap(),
        );
    }

    #[test]
    fn dead_server_is_gone_then_respawns() {
        let root = temp_root("dead");
        let Some(argv) = server_argv(&root) else { return };
        let fs = SftpFs::spawn(argv).unwrap();
        assert!(fs.list(&root).is_ok());
        fs.kill_for_test();
        assert!(matches!(fs.list(&root), Err(FsError::Gone(_))));
        assert!(fs.list(&root).is_ok());
    }
}
