//! Saving a file without ever leaving it half-written: write a temp file next to it, give it
//! the file's mode, and rename it over the file. When a rename can't keep the file as it was
//! (a folder we can't write, another owner, hard links), say so instead; the caller can then
//! write in place after keeping a local backup.

use std::path::Path;
use std::time::SystemTime;

use crate::{FileEntry, FileSystem, FsError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub size: u64,
    pub modified: Option<SystemTime>,
}

pub fn version_of(e: &FileEntry) -> Version {
    Version { size: e.size.unwrap_or(0), modified: e.modified }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InPlaceReason {
    FolderNotWritable,
    OtherOwner,
    HardLinks,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveError {
    /// The file changed since `expected` was read.
    Changed { now: Version },
    NeedsInPlace(InPlaceReason),
    Fs(FsError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Saved {
    pub version: Version,
    /// The file written (a link's target).
    pub path: String,
    /// The server couldn't rename over the file: it was removed and the temp renamed.
    pub fallback_note: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Eol {
    Lf,
    Crlf,
}

fn split(real: &str) -> (&str, &str) {
    match real.rfind('/') {
        Some(0) => ("/", &real[1..]),
        Some(i) => (&real[..i], &real[i + 1..]),
        None => (".", real),
    }
}

fn temp_name(dir: &str, name: &str) -> String {
    let mut b = [0u8; 4];
    let _ = getrandom::fill(&mut b);
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    let dir = if dir == "/" { "" } else { dir };
    format!("{dir}/.{name}.rexa-{hex}.tmp")
}

fn check(cur: &FileEntry, expected: Option<&Version>) -> Result<(), SaveError> {
    match expected {
        Some(exp) if &version_of(cur) != exp => Err(SaveError::Changed { now: version_of(cur) }),
        _ => Ok(()),
    }
}

/// Saves `bytes` to `path` (or the file its links lead to) by temp file and rename, after
/// checking it is still the `expected` version. A missing file is created when no version is
/// expected. On any error the file is as it was and the temp file is gone.
pub fn safe_save(fs: &dyn FileSystem, path: &str, bytes: &[u8], expected: Option<&Version>) -> Result<Saved, SaveError> {
    let real = match fs.resolve(path) {
        Ok(r) => r,
        Err(FsError::NotFound) if expected.is_none() => {
            fs.write_file(path, bytes, true).map_err(SaveError::Fs)?;
            let version = version_of(&fs.stat_target(path).map_err(SaveError::Fs)?);
            return Ok(Saved { version, path: path.to_string(), fallback_note: false });
        }
        Err(e) => return Err(SaveError::Fs(e)),
    };
    let cur = fs.stat_target(&real).map_err(SaveError::Fs)?;
    check(&cur, expected)?;
    if cur.nlink.is_some_and(|n| n > 1) {
        return Err(SaveError::NeedsInPlace(InPlaceReason::HardLinks));
    }
    let (dir, name) = split(&real);
    let tmp = temp_name(dir, name);
    match fs.write_file(&tmp, bytes, true) {
        Ok(()) => {}
        Err(FsError::Denied) => return Err(SaveError::NeedsInPlace(InPlaceReason::FolderNotWritable)),
        Err(e) => {
            let _ = fs.remove(&tmp);
            return Err(SaveError::Fs(e));
        }
    }
    let drop_tmp = |e: SaveError| {
        let _ = fs.remove(&tmp);
        e
    };
    if let Some(mode) = cur.mode {
        fs.set_mode(&tmp, mode & 0o7777).map_err(|e| drop_tmp(SaveError::Fs(e)))?;
    }
    let t = fs.stat(&tmp).map_err(|e| drop_tmp(SaveError::Fs(e)))?;
    let differs = |a: Option<u32>, b: Option<u32>| matches!((a, b), (Some(x), Some(y)) if x != y);
    if differs(cur.uid, t.uid) || differs(cur.gid, t.gid) {
        return Err(drop_tmp(SaveError::NeedsInPlace(InPlaceReason::OtherOwner)));
    }
    let mut fallback_note = false;
    match fs.replace(&tmp, &real) {
        Ok(()) => {}
        Err(FsError::Unsupported(_)) => {
            fs.remove(&real).map_err(|e| drop_tmp(SaveError::Fs(e)))?;
            // The old file is gone: keep the temp (it holds the new text) if this fails.
            fs.rename(&tmp, &real).map_err(|e| SaveError::Fs(FsError::Other(format!("{e}; the new text is in {tmp}"))))?;
            fallback_note = true;
        }
        Err(e) => return Err(drop_tmp(SaveError::Fs(e))),
    }
    let version = version_of(&fs.stat_target(&real).map_err(SaveError::Fs)?);
    Ok(Saved { version, path: real, fallback_note })
}

/// Writes over the file itself (keeping its inode, owner and links) after copying what it held
/// to `backup_dir` on this computer; puts the old bytes back if the write fails.
pub fn save_in_place(
    fs: &dyn FileSystem,
    path: &str,
    bytes: &[u8],
    expected: Option<&Version>,
    backup_dir: &Path,
    machine: &str,
) -> Result<Saved, SaveError> {
    let real = fs.resolve(path).map_err(SaveError::Fs)?;
    let cur = fs.stat_target(&real).map_err(SaveError::Fs)?;
    check(&cur, expected)?;
    let old = fs.read(&real, u64::MAX).map_err(SaveError::Fs)?;
    let stamp = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let safe = |s: &str| s.replace(['/', ':', '\\'], "_");
    let backup = backup_dir.join(format!("{stamp}-{}-{}", safe(machine), safe(split(&real).1)));
    std::fs::create_dir_all(backup_dir)
        .and_then(|_| std::fs::write(&backup, &old))
        .map_err(|e| SaveError::Fs(FsError::Other(format!("backup {}: {e}", backup.display()))))?;
    if let Err(e) = fs.write_file(&real, bytes, false) {
        let _ = fs.write_file(&real, &old, false);
        return Err(SaveError::Fs(e));
    }
    let version = version_of(&fs.stat_target(&real).map_err(SaveError::Fs)?);
    Ok(Saved { version, path: real, fallback_note: false })
}

/// CRLF when the first line break is one.
pub fn detect_eol(text: &str) -> Eol {
    match text.find('\n') {
        Some(i) if i > 0 && text.as_bytes()[i - 1] == b'\r' => Eol::Crlf,
        _ => Eol::Lf,
    }
}

/// The editor's LF text with the file's line breaks.
pub fn with_eol(text_lf: &str, eol: Eol) -> String {
    match eol {
        Eol::Lf => text_lf.to_string(),
        Eol::Crlf => text_lf.replace('\n', "\r\n"),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::local::LocalFs;
    use crate::{Kind, Listing};
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    fn root(name: &str) -> String {
        let d = std::env::temp_dir().join(format!("rexa-save-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d.to_string_lossy().into_owned()
    }

    fn version(fs_: &dyn FileSystem, p: &str) -> Version {
        version_of(&fs_.stat_target(p).unwrap())
    }

    fn temps(dir: &str) -> Vec<String> {
        fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).filter(|n| n.contains(".rexa-")).collect()
    }

    /// The cases every file system must handle the same way (the root is a local folder).
    pub(crate) fn save_cases(f: &dyn FileSystem, root: &str) {
        // Mode kept, no temp left.
        let env = format!("{root}/.env");
        fs::write(&env, b"KEY=old\n").unwrap();
        fs::set_permissions(&env, fs::Permissions::from_mode(0o600)).unwrap();
        let v = version(f, &env);
        let saved = safe_save(f, &env, b"KEY=new\n", Some(&v)).unwrap();
        assert_eq!(fs::read(&env).unwrap(), b"KEY=new\n");
        assert_eq!(fs::metadata(&env).unwrap().permissions().mode() & 0o777, 0o600);
        assert!(temps(root).is_empty(), "{:?}", temps(root));
        // The new version is the base for the next save: no false conflict.
        safe_save(f, &env, b"KEY=newer\n", Some(&saved.version)).unwrap();

        // Through links: the target is written, every link kept.
        let target = format!("{root}/real it's.txt");
        fs::write(&target, b"one").unwrap();
        std::os::unix::fs::symlink("real it's.txt", format!("{root}/rel")).unwrap();
        std::os::unix::fs::symlink(format!("{root}/rel"), format!("{root}/link2")).unwrap();
        let v = version(f, &format!("{root}/link2"));
        let saved = safe_save(f, &format!("{root}/link2"), b"two", Some(&v)).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"two");
        assert!(fs::symlink_metadata(format!("{root}/rel")).unwrap().file_type().is_symlink());
        assert!(fs::symlink_metadata(format!("{root}/link2")).unwrap().file_type().is_symlink());
        assert_eq!(saved.path, fs::canonicalize(&target).unwrap().to_string_lossy());

        // Changed on disk since opened: refused; with the new version: saved.
        let doc = format!("{root}/doc.txt");
        fs::write(&doc, b"v1").unwrap();
        let v = version(f, &doc);
        fs::write(&doc, b"v2 by someone else").unwrap();
        match safe_save(f, &doc, b"mine", Some(&v)) {
            Err(SaveError::Changed { now }) => {
                assert_eq!(fs::read(&doc).unwrap(), b"v2 by someone else");
                safe_save(f, &doc, b"mine", Some(&now)).unwrap();
                assert_eq!(fs::read(&doc).unwrap(), b"mine");
            }
            other => panic!("{other:?}"),
        }

        // A file that isn't there yet is created.
        safe_save(f, &format!("{root}/new.txt"), b"fresh", None).unwrap();
        assert_eq!(fs::read(format!("{root}/new.txt")).unwrap(), b"fresh");

        // A folder we can't write: ask, and leave the file alone.
        let ro = format!("{root}/ro");
        fs::create_dir(&ro).unwrap();
        fs::write(format!("{ro}/f"), b"keep").unwrap();
        fs::set_permissions(&ro, fs::Permissions::from_mode(0o555)).unwrap();
        let v = version(f, &format!("{ro}/f"));
        let r = safe_save(f, &format!("{ro}/f"), b"nope", Some(&v));
        let after = fs::read(format!("{ro}/f")).unwrap();
        // In place: same inode, backup kept.
        let backups = std::path::PathBuf::from(format!("{root}/backups"));
        let ino = fs::metadata(format!("{ro}/f")).unwrap().ino();
        let ip = save_in_place(f, &format!("{ro}/f"), b"in place", Some(&v), &backups, "m");
        fs::set_permissions(&ro, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(r, Err(SaveError::NeedsInPlace(InPlaceReason::FolderNotWritable)));
        assert_eq!(after, b"keep");
        ip.unwrap();
        assert_eq!(fs::read(format!("{ro}/f")).unwrap(), b"in place");
        assert_eq!(fs::metadata(format!("{ro}/f")).unwrap().ino(), ino);
        let kept: Vec<_> = fs::read_dir(&backups).unwrap().map(|e| fs::read(e.unwrap().path()).unwrap()).collect();
        assert_eq!(kept, vec![b"keep".to_vec()]);
    }

    #[test]
    fn local_save_cases() {
        save_cases(&LocalFs, &root("local"));
    }

    #[test]
    fn hard_links_need_in_place() {
        let r = root("hard");
        let a = format!("{r}/a");
        fs::write(&a, b"x").unwrap();
        fs::hard_link(&a, format!("{r}/b")).unwrap();
        let v = version(&LocalFs, &a);
        assert_eq!(safe_save(&LocalFs, &a, b"y", Some(&v)), Err(SaveError::NeedsInPlace(InPlaceReason::HardLinks)));
        assert!(temps(&r).is_empty());
    }

    /// Local files with one thing changed.
    struct Twist {
        other_owner: bool,
        replace: Option<FsError>,
    }

    impl FileSystem for Twist {
        fn home(&self) -> Result<String, FsError> { LocalFs.home() }
        fn list(&self, p: &str) -> Result<Listing, FsError> { LocalFs.list(p) }
        fn stat(&self, p: &str) -> Result<FileEntry, FsError> {
            let mut e = LocalFs.stat(p)?;
            if self.other_owner && p.contains(".rexa-") {
                e.uid = e.uid.map(|u| u + 1);
            }
            Ok(e)
        }
        fn stat_target(&self, p: &str) -> Result<FileEntry, FsError> { LocalFs.stat_target(p) }
        fn read(&self, p: &str, m: u64) -> Result<Vec<u8>, FsError> { LocalFs.read(p, m) }
        fn create_file(&self, p: &str) -> Result<(), FsError> { LocalFs.create_file(p) }
        fn create_dir(&self, p: &str) -> Result<(), FsError> { LocalFs.create_dir(p) }
        fn rename(&self, a: &str, b: &str) -> Result<(), FsError> { LocalFs.rename(a, b) }
        fn remove(&self, p: &str) -> Result<(), FsError> { LocalFs.remove(p) }
        fn writable(&self) -> Result<(), String> { Ok(()) }
        fn resolve(&self, p: &str) -> Result<String, FsError> { LocalFs.resolve(p) }
        fn write_file(&self, p: &str, b: &[u8], n: bool) -> Result<(), FsError> { LocalFs.write_file(p, b, n) }
        fn set_mode(&self, p: &str, m: u32) -> Result<(), FsError> { LocalFs.set_mode(p, m) }
        fn replace(&self, a: &str, b: &str) -> Result<(), FsError> {
            match &self.replace {
                Some(e) => Err(e.clone()),
                None => LocalFs.replace(a, b),
            }
        }
    }

    #[test]
    fn other_owner_needs_in_place() {
        let r = root("owner");
        let p = format!("{r}/f");
        fs::write(&p, b"x").unwrap();
        let t = Twist { other_owner: true, replace: None };
        let v = version(&t, &p);
        assert_eq!(safe_save(&t, &p, b"y", Some(&v)), Err(SaveError::NeedsInPlace(InPlaceReason::OtherOwner)));
        assert_eq!(fs::read(&p).unwrap(), b"x");
        assert!(temps(&r).is_empty());
    }

    #[test]
    fn failed_replace_leaves_the_file() {
        let r = root("failrep");
        let p = format!("{r}/f");
        fs::write(&p, b"x").unwrap();
        let t = Twist { other_owner: false, replace: Some(FsError::Gone("cut".into())) };
        let v = version(&t, &p);
        assert_eq!(safe_save(&t, &p, b"y", Some(&v)), Err(SaveError::Fs(FsError::Gone("cut".into()))));
        assert_eq!(fs::read(&p).unwrap(), b"x");
        assert!(temps(&r).is_empty());
    }

    #[test]
    fn unsupported_replace_falls_back() {
        let r = root("fallback");
        let p = format!("{r}/f");
        fs::write(&p, b"x").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o640)).unwrap();
        let t = Twist { other_owner: false, replace: Some(FsError::Unsupported("no".into())) };
        let v = version(&t, &p);
        let s = safe_save(&t, &p, b"y", Some(&v)).unwrap();
        assert!(s.fallback_note);
        assert_eq!(fs::read(&p).unwrap(), b"y");
        assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o640);
        assert!(temps(&r).is_empty());
        let _ = Kind::File;
    }

    #[test]
    fn eol_round_trip() {
        let crlf = "a\r\nb\r\nc";
        assert_eq!(detect_eol(crlf), Eol::Crlf);
        assert_eq!(detect_eol("a\nb\r\n"), Eol::Lf);
        assert_eq!(detect_eol("single line"), Eol::Lf);
        let lf = crlf.replace("\r\n", "\n");
        assert_eq!(with_eol(&lf, Eol::Crlf), crlf);
        assert_eq!(with_eol("x\ny\n", Eol::Crlf), "x\r\ny\r\n");
        assert_eq!(with_eol("x\ny", Eol::Lf), "x\ny");
    }
}
