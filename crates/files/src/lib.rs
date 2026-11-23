//! Files on this computer, on ssh machines (SFTP) and on machines Rex reaches, behind one
//! blocking interface. Paths are POSIX strings; callers run calls off the UI thread.

pub mod local;
pub mod rex;
pub mod sftp;
#[cfg(test)]
pub(crate) mod conformance;

use std::fmt;
use std::time::SystemTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Dir,
    File,
    Symlink { to_dir: bool },
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub name: String,
    pub kind: Kind,
    pub size: Option<u64>,
    pub modified: Option<SystemTime>,
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    /// Hard links to the file (None when the source can't tell).
    pub nlink: Option<u64>,
}

impl FileEntry {
    /// A directory, or a link to one: something to open as a folder.
    pub fn is_dir(&self) -> bool {
        matches!(self.kind, Kind::Dir | Kind::Symlink { to_dir: true })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    /// The directory listed, absolute.
    pub path: String,
    pub entries: Vec<FileEntry>,
    /// Some entries were left out.
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsError {
    NotFound,
    Exists,
    Denied,
    NotDir,
    /// This source can't change files; why.
    ReadOnly(String),
    /// The connection is gone; the next call tries a new one.
    Gone(String),
    /// The server can't do this (e.g. rename over a file).
    Unsupported(String),
    Other(String),
}

impl fmt::Display for FsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FsError::NotFound => f.write_str("not found"),
            FsError::Exists => f.write_str("already exists"),
            FsError::Denied => f.write_str("permission denied"),
            FsError::NotDir => f.write_str("not a directory"),
            FsError::ReadOnly(why) => write!(f, "read-only: {why}"),
            FsError::Gone(why) => write!(f, "connection lost: {why}"),
            FsError::Unsupported(why) => write!(f, "not supported: {why}"),
            FsError::Other(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for FsError {}

pub trait FileSystem: Send + Sync {
    /// The user's home directory there.
    fn home(&self) -> Result<String, FsError>;
    fn list(&self, path: &str) -> Result<Listing, FsError>;
    /// The entry itself (a link is not followed).
    fn stat(&self, path: &str) -> Result<FileEntry, FsError>;
    /// What the path leads to, links followed: Other for pipes, devices and sockets.
    fn stat_target(&self, path: &str) -> Result<FileEntry, FsError>;
    /// At most `max` bytes from the start of a file.
    fn read(&self, path: &str, max: u64) -> Result<Vec<u8>, FsError>;
    /// A new empty file; Exists if anything is there.
    fn create_file(&self, path: &str) -> Result<(), FsError>;
    fn create_dir(&self, path: &str) -> Result<(), FsError>;
    /// Exists if `to` is there: never replaces.
    fn rename(&self, from: &str, to: &str) -> Result<(), FsError>;
    /// A file or link; a directory with everything in it.
    fn remove(&self, path: &str) -> Result<(), FsError>;
    /// Ok if files can be changed, else why not.
    fn writable(&self) -> Result<(), String>;
    /// The absolute path with every link followed; NotFound when it leads nowhere.
    fn resolve(&self, path: &str) -> Result<String, FsError>;
    /// Writes a whole file: a new one (Exists if anything is there) or over an existing one.
    fn write_file(&self, path: &str, bytes: &[u8], create_new: bool) -> Result<(), FsError>;
    /// Sets the permission bits.
    fn set_mode(&self, path: &str, mode: u32) -> Result<(), FsError>;
    /// Renames `from` to `to` in one step, replacing `to`; Unsupported when the server can't.
    fn replace(&self, from: &str, to: &str) -> Result<(), FsError>;
}

/// `dir` + "/" + `name`.
pub fn join(dir: &str, name: &str) -> String {
    let dir = dir.trim_end_matches('/');
    format!("{dir}/{name}")
}

/// The directory a path is in; None for "/".
pub fn parent(path: &str) -> Option<String> {
    let path = path.trim_end_matches('/');
    if path.is_empty() {
        return None;
    }
    match path.rfind('/') {
        Some(0) => Some("/".into()),
        Some(i) => Some(path[..i].to_string()),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_and_parent() {
        assert_eq!(join("/a", "b"), "/a/b");
        assert_eq!(join("/a/", "b"), "/a/b");
        assert_eq!(join("/", "b"), "/b");
        assert_eq!(parent("/a/b"), Some("/a".into()));
        assert_eq!(parent("/a/b/"), Some("/a".into()));
        assert_eq!(parent("/a"), Some("/".into()));
        assert_eq!(parent("/"), None);
    }
}
