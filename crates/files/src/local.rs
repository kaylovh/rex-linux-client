//! This computer's files.

use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use crate::{FileEntry, FileSystem, FsError, Kind, Listing};

pub struct LocalFs;

fn err(e: io::Error) -> FsError {
    match e.kind() {
        io::ErrorKind::NotFound => FsError::NotFound,
        io::ErrorKind::AlreadyExists => FsError::Exists,
        io::ErrorKind::PermissionDenied => FsError::Denied,
        io::ErrorKind::NotADirectory => FsError::NotDir,
        _ => FsError::Other(e.to_string()),
    }
}

fn entry(name: String, path: &Path) -> Result<FileEntry, FsError> {
    let meta = fs::symlink_metadata(path).map_err(err)?;
    let ft = meta.file_type();
    let kind = if ft.is_symlink() {
        Kind::Symlink { to_dir: fs::metadata(path).is_ok_and(|m| m.is_dir()) }
    } else if ft.is_dir() {
        Kind::Dir
    } else if ft.is_file() {
        Kind::File
    } else {
        Kind::Other
    };
    Ok(FileEntry {
        name,
        kind,
        size: Some(meta.len()),
        modified: meta.modified().ok(),
        mode: Some(meta.permissions().mode()),
    })
}

fn name_of(path: &str) -> String {
    Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.to_string())
}

impl FileSystem for LocalFs {
    fn home(&self) -> Result<String, FsError> {
        std::env::var("HOME").map_err(|_| FsError::Other("HOME is not set".into()))
    }

    fn list(&self, path: &str) -> Result<Listing, FsError> {
        let abs = fs::canonicalize(path).map_err(err)?;
        if !abs.is_dir() {
            return Err(FsError::NotDir);
        }
        let mut entries = Vec::new();
        for e in fs::read_dir(&abs).map_err(err)? {
            let e = e.map_err(err)?;
            let name = e.file_name().to_string_lossy().into_owned();
            if let Ok(en) = entry(name, &e.path()) {
                entries.push(en);
            }
        }
        Ok(Listing { path: abs.to_string_lossy().into_owned(), entries, truncated: false })
    }

    fn stat(&self, path: &str) -> Result<FileEntry, FsError> {
        entry(name_of(path), Path::new(path))
    }

    fn read(&self, path: &str, max: u64) -> Result<Vec<u8>, FsError> {
        let mut out = Vec::new();
        fs::File::open(path).map_err(err)?.take(max).read_to_end(&mut out).map_err(err)?;
        Ok(out)
    }

    fn create_file(&self, path: &str) -> Result<(), FsError> {
        fs::OpenOptions::new().write(true).create_new(true).open(path).map(|_| ()).map_err(err)
    }

    fn create_dir(&self, path: &str) -> Result<(), FsError> {
        fs::create_dir(path).map_err(err)
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), FsError> {
        if fs::symlink_metadata(to).is_ok() {
            return Err(FsError::Exists);
        }
        fs::symlink_metadata(from).map_err(err)?;
        fs::rename(from, to).map_err(err)
    }

    fn remove(&self, path: &str) -> Result<(), FsError> {
        let meta = fs::symlink_metadata(path).map_err(err)?;
        if meta.is_dir() { fs::remove_dir_all(path) } else { fs::remove_file(path) }.map_err(err)
    }

    fn writable(&self) -> Result<(), String> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn temp_root(name: &str) -> String {
        let d = std::env::temp_dir().join(format!("rexa-files-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d.to_string_lossy().into_owned()
    }

    #[test]
    fn local_conformance() {
        let root = temp_root("local");
        crate::conformance::run(
            &LocalFs,
            &root,
            &|p, b| fs::write(p, b).unwrap(),
            &|target, p| std::os::unix::fs::symlink(target, p).unwrap(),
        );
    }
}
