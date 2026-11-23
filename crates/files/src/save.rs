//! Saving a file without ever leaving it half-written: write a temp file next to it, give it
//! the file's mode, and rename it over the file. When a rename can't keep the file as it was
//! (a folder we can't write, another owner, hard links), say so instead; the caller can then
//! write in place after keeping a local backup.

use std::path::Path;
use std::time::SystemTime;

use crate::{FileEntry, FileSystem, FsError};

/// What a file was when it was read or saved: size and time, and a hash of its contents when
/// known (times can be whole seconds, so a same-size change within one is caught by it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub size: u64,
    /// None: unknown (the file was written but couldn't be looked at after).
    pub modified: Option<SystemTime>,
    pub hash: Option<u64>,
}

pub fn version_of(e: &FileEntry) -> Version {
    Version { size: e.size.unwrap_or(0), modified: e.modified, hash: None }
}

fn hash(bytes: &[u8]) -> u64 {
    let mut h = std::hash::DefaultHasher::new();
    std::hash::Hash::hash(bytes, &mut h);
    std::hash::Hasher::finish(&h)
}

impl Version {
    /// The version of a file whose entry and contents were just read.
    pub fn read(e: &FileEntry, bytes: &[u8]) -> Version {
        Version { hash: Some(hash(bytes)), ..version_of(e) }
    }
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
    /// Writing in place failed; whether the old contents were put back, and where the
    /// backup of them is.
    InPlaceFailed { error: FsError, restored: bool, backup: std::path::PathBuf },
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

fn random_hex() -> Result<String, SaveError> {
    let mut b = [0u8; 4];
    getrandom::fill(&mut b).map_err(|e| SaveError::Fs(FsError::Other(format!("random: {e}"))))?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

fn temp_name(dir: &str, name: &str) -> Result<String, SaveError> {
    let dir = if dir == "/" { "" } else { dir };
    Ok(format!("{dir}/.{name}.rexa-{}.tmp", random_hex()?))
}

/// The file is still the expected version: same size and time (when known), and the same
/// contents when a hash was taken.
fn check(fs: &dyn FileSystem, real: &str, cur: &FileEntry, expected: Option<&Version>) -> Result<(), SaveError> {
    let Some(exp) = expected else { return Ok(()) };
    let now = version_of(cur);
    let changed = now.size != exp.size || (exp.modified.is_some() && now.modified != exp.modified);
    if changed {
        return Err(SaveError::Changed { now });
    }
    if let Some(h) = exp.hash {
        let bytes = fs.read(real, exp.size.saturating_add(1)).map_err(SaveError::Fs)?;
        let now = Version::read(cur, &bytes);
        if now.hash != Some(h) {
            return Err(SaveError::Changed { now });
        }
    }
    Ok(())
}

/// Creates a new file; something already there (made since the editor opened) is a change,
/// never overwritten.
pub fn create_new(fs: &dyn FileSystem, path: &str, bytes: &[u8]) -> Result<Saved, SaveError> {
    match fs.write_file(path, bytes, true, None) {
        Ok(()) => {
            let version = fs.stat_target(path).map(|e| Version::read(&e, bytes)).unwrap_or(Version { size: bytes.len() as u64, modified: None, hash: Some(hash(bytes)) });
            Ok(Saved { version, path: path.to_string(), fallback_note: false })
        }
        Err(FsError::Exists) => {
            let now = fs.stat_target(path).map(|e| version_of(&e)).map_err(SaveError::Fs)?;
            Err(SaveError::Changed { now })
        }
        Err(e) => Err(SaveError::Fs(e)),
    }
}

/// Saves `bytes` to `path` (or the file its links lead to) by temp file and rename, after
/// checking it is still the `expected` version. A missing file is created when no version is
/// expected. On any error the file is as it was and the temp file is gone.
pub fn safe_save(fs: &dyn FileSystem, path: &str, bytes: &[u8], expected: Option<&Version>) -> Result<Saved, SaveError> {
    let real = match fs.resolve(path) {
        Ok(r) => r,
        Err(FsError::NotFound) if expected.is_none() => return create_new(fs, path, bytes),
        Err(e) => return Err(SaveError::Fs(e)),
    };
    let cur = fs.stat_target(&real).map_err(SaveError::Fs)?;
    check(fs, &real, &cur, expected)?;
    if fs.link_count(&real).map_err(SaveError::Fs)?.is_some_and(|n| n > 1) {
        return Err(SaveError::NeedsInPlace(InPlaceReason::HardLinks));
    }
    let (dir, name) = split(&real);
    let tmp = temp_name(dir, name)?;
    // Private from its first byte: it may hold what a 600 file holds.
    match fs.write_file(&tmp, bytes, true, Some(0o600)) {
        Ok(()) => {}
        // Not ours: never removed.
        Err(FsError::Exists) => return Err(SaveError::Fs(FsError::Exists)),
        Err(FsError::Denied) => {
            let _ = fs.remove(&tmp);
            return Err(SaveError::NeedsInPlace(InPlaceReason::FolderNotWritable));
        }
        Err(e) => {
            let _ = fs.remove(&tmp);
            return Err(SaveError::Fs(e));
        }
    }
    // Removes the temp; when it can't be removed, the error says where it is.
    let drop_tmp = |e: SaveError| match fs.remove(&tmp) {
        Ok(()) => e,
        Err(_) => match e {
            SaveError::Fs(inner) => SaveError::Fs(FsError::Other(format!("{inner}; a private temporary copy is left at {tmp}"))),
            other => other,
        },
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
    // Saved: if the file can't be looked at now, its version is what was written.
    let version = fs
        .stat_target(&real)
        .map(|e| Version::read(&e, bytes))
        .unwrap_or(Version { size: bytes.len() as u64, modified: None, hash: Some(hash(bytes)) });
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
    check(fs, &real, &cur, expected)?;
    let old = fs.read(&real, u64::MAX).map_err(SaveError::Fs)?;
    let stamp = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let safe = |s: &str| s.replace(['/', ':', '\\'], "_");
    let backup = backup_dir.join(format!("{stamp}-{}-{}-{}", random_hex()?, safe(machine), safe(split(&real).1)));
    write_private(backup_dir, &backup, &old).map_err(|e| SaveError::Fs(FsError::Other(format!("backup {}: {e}", backup.display()))))?;
    if let Err(error) = fs.write_file(&real, bytes, false, None) {
        let restored = fs.write_file(&real, &old, false, None).is_ok();
        return Err(SaveError::InPlaceFailed { error, restored, backup });
    }
    let version = fs
        .stat_target(&real)
        .map(|e| Version::read(&e, bytes))
        .unwrap_or(Version { size: bytes.len() as u64, modified: None, hash: Some(hash(bytes)) });
    Ok(Saved { version, path: real, fallback_note: false })
}

/// Writes a backup only its owner can read, in a folder only its owner can enter.
fn write_private(dir: &Path, file: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(file)?;
    f.write_all(bytes)?;
    f.sync_all()
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
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

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
        let kept: Vec<_> = fs::read_dir(&backups).unwrap().map(|e| e.unwrap().path()).collect();
        assert_eq!(kept.iter().map(|p| fs::read(p).unwrap()).collect::<Vec<_>>(), vec![b"keep".to_vec()]);
        assert_eq!(fs::metadata(&kept[0]).unwrap().permissions().mode() & 0o777, 0o600);

        // Hard links: replacing would split them.
        let a = format!("{root}/linked");
        fs::write(&a, b"x").unwrap();
        fs::hard_link(&a, format!("{root}/linked2")).unwrap();
        let v = version(f, &a);
        assert_eq!(safe_save(f, &a, b"y", Some(&v)), Err(SaveError::NeedsInPlace(InPlaceReason::HardLinks)));
        assert!(temps(root).is_empty());
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
    #[derive(Default)]
    struct Twist {
        other_owner: bool,
        replace: Option<FsError>,
        fail_set_mode: bool,
        fail_remove: bool,
        /// Creating the temp says something is already there.
        tmp_exists: bool,
        /// The next write over a non-temp file fails after truncating it.
        fail_real_write: AtomicBool,
        removes: AtomicU32,
        /// stat_target fails once the replace happened.
        lose_after_replace: bool,
        replaced: AtomicBool,
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
        fn stat_target(&self, p: &str) -> Result<FileEntry, FsError> {
            if self.lose_after_replace && self.replaced.load(Ordering::SeqCst) {
                return Err(FsError::Gone("cut".into()));
            }
            LocalFs.stat_target(p)
        }
        fn read(&self, p: &str, m: u64) -> Result<Vec<u8>, FsError> { LocalFs.read(p, m) }
        fn create_file(&self, p: &str) -> Result<(), FsError> { LocalFs.create_file(p) }
        fn create_dir(&self, p: &str) -> Result<(), FsError> { LocalFs.create_dir(p) }
        fn rename(&self, a: &str, b: &str) -> Result<(), FsError> { LocalFs.rename(a, b) }
        fn remove(&self, p: &str) -> Result<(), FsError> {
            self.removes.fetch_add(1, Ordering::SeqCst);
            if self.fail_remove { Err(FsError::Gone("cut".into())) } else { LocalFs.remove(p) }
        }
        fn writable(&self) -> Result<(), String> { Ok(()) }
        fn resolve(&self, p: &str) -> Result<String, FsError> { LocalFs.resolve(p) }
        fn write_file(&self, p: &str, b: &[u8], n: bool, mode: Option<u32>) -> Result<(), FsError> {
            if self.tmp_exists && p.contains(".rexa-") {
                return Err(FsError::Exists);
            }
            if !p.contains(".rexa-") && !n && self.fail_real_write.swap(false, Ordering::SeqCst) {
                LocalFs.write_file(p, &b[..b.len() / 2], false, None)?;
                return Err(FsError::Gone("cut".into()));
            }
            LocalFs.write_file(p, b, n, mode)
        }
        fn set_mode(&self, p: &str, m: u32) -> Result<(), FsError> {
            if self.fail_set_mode { Err(FsError::Gone("cut".into())) } else { LocalFs.set_mode(p, m) }
        }
        fn link_count(&self, p: &str) -> Result<Option<u64>, FsError> { LocalFs.link_count(p) }
        fn replace(&self, a: &str, b: &str) -> Result<(), FsError> {
            match &self.replace {
                Some(e) => Err(e.clone()),
                None => {
                    LocalFs.replace(a, b)?;
                    self.replaced.store(true, Ordering::SeqCst);
                    Ok(())
                }
            }
        }
    }

    #[test]
    fn a_left_temp_is_private_and_named() {
        let r = root("private");
        let p = format!("{r}/.env");
        fs::write(&p, b"SECRET=1").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o600)).unwrap();
        let t = Twist { fail_set_mode: true, fail_remove: true, ..Default::default() };
        let v = version(&t, &p);
        let err = safe_save(&t, &p, b"SECRET=2", Some(&v)).unwrap_err();
        let left = temps(&r);
        assert_eq!(left.len(), 1);
        assert_eq!(fs::metadata(format!("{r}/{}", left[0])).unwrap().permissions().mode() & 0o777, 0o600);
        assert!(format!("{err:?}").contains(&left[0]), "{err:?}");
        assert_eq!(fs::read(&p).unwrap(), b"SECRET=1");
    }

    #[test]
    fn a_temp_name_taken_is_not_deleted() {
        let r = root("taken");
        let p = format!("{r}/f");
        fs::write(&p, b"x").unwrap();
        let t = Twist { tmp_exists: true, ..Default::default() };
        let v = version(&t, &p);
        assert!(safe_save(&t, &p, b"y", Some(&v)).is_err());
        assert_eq!(t.removes.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_failed_in_place_write_is_restored_and_says_where_the_backup_is() {
        let r = root("inplacefail");
        let p = format!("{r}/f");
        fs::write(&p, b"original contents").unwrap();
        let t = Twist { fail_real_write: AtomicBool::new(true), ..Default::default() };
        let v = version(&t, &p);
        let backups = std::path::PathBuf::from(format!("{r}/backups"));
        match save_in_place(&t, &p, b"new contents here", Some(&v), &backups, "m") {
            Err(SaveError::InPlaceFailed { restored, backup, .. }) => {
                assert!(restored);
                assert_eq!(fs::read(&backup).unwrap(), b"original contents");
                assert_eq!(fs::metadata(&backup).unwrap().permissions().mode() & 0o777, 0o600);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(fs::read(&p).unwrap(), b"original contents");
    }

    #[test]
    fn saved_but_unverified_is_still_saved() {
        let r = root("unverified");
        let p = format!("{r}/f");
        fs::write(&p, b"x").unwrap();
        let t = Twist { lose_after_replace: true, ..Default::default() };
        let v = version(&t, &p);
        let s = safe_save(&t, &p, b"yy", Some(&v)).unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"yy");
        // The next save (with the file as written) sees no conflict.
        safe_save(&LocalFs, &p, b"zz", Some(&s.version)).unwrap();
    }

    #[test]
    fn a_new_file_never_overwrites() {
        let r = root("newfile");
        let p = format!("{r}/notes.md");
        create_new(&LocalFs, &p, b"mine").unwrap();
        assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o077, 0o044 & fs::metadata(&p).unwrap().permissions().mode());
        fs::write(&p, b"theirs").unwrap();
        assert!(matches!(create_new(&LocalFs, &format!("{r}/notes.md"), b"again"), Err(SaveError::Changed { .. })));
        assert_eq!(fs::read(&p).unwrap(), b"theirs");
    }

    #[test]
    fn same_size_change_in_the_same_second_is_seen() {
        let r = root("samesecond");
        let p = format!("{r}/f");
        fs::write(&p, b"aaaa").unwrap();
        let e = LocalFs.stat_target(&p).unwrap();
        let v = Version::read(&e, b"aaaa");
        fs::write(&p, b"bbbb").unwrap();
        fs::File::options().write(true).open(&p).unwrap().set_modified(e.modified.unwrap()).unwrap();
        assert!(matches!(safe_save(&LocalFs, &p, b"cccc", Some(&v)), Err(SaveError::Changed { .. })));
        assert_eq!(fs::read(&p).unwrap(), b"bbbb");
    }

    #[test]
    fn other_owner_needs_in_place() {
        let r = root("owner");
        let p = format!("{r}/f");
        fs::write(&p, b"x").unwrap();
        let t = Twist { other_owner: true, ..Default::default() };
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
        let t = Twist { replace: Some(FsError::Gone("cut".into())), ..Default::default() };
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
        let t = Twist { replace: Some(FsError::Unsupported("no".into())), ..Default::default() };
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
