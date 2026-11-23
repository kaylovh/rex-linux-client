//! Copying a file or a folder between two file systems through `<name>.part` files: the
//! name appears only when everything arrived, and an interrupted copy continues from the
//! part's size.

use crate::{FileSystem, FsError, Kind, join};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub done: u64,
    pub total: u64,
}

/// What the progress callback says: go on, stop and throw the part away, or stop and keep
/// it for a later resume (the app is quitting).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Go,
    Cancel,
    Abort,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopyError {
    /// Stopped by the caller; the part file is gone.
    Canceled,
    /// Stopped by the caller; the part file is kept for a resume.
    Aborted,
    /// The target exists (and wasn't to be overwritten); the part file is kept.
    Exists,
    Fs(FsError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    Dir,
    File,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// Relative to the folder the copy goes into, starting with the copied name.
    pub rel: String,
    pub kind: ItemKind,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Plan {
    pub items: Vec<Item>,
    pub total: u64,
    /// Links and special files left out.
    pub skipped: usize,
}

pub fn part_path(path: &str) -> String {
    format!("{path}.part")
}

/// What copying `path` takes: the file, or the folder and everything in it (links and
/// special files are left out and counted).
pub fn plan_tree(fs: &dyn FileSystem, path: &str) -> Result<Plan, FsError> {
    // A link given itself is followed (links inside a folder are not).
    let top = match fs.stat(path)? {
        e if matches!(e.kind, Kind::Symlink { .. }) => fs.stat_target(path)?,
        e => e,
    };
    let name = path.trim_end_matches('/').rsplit('/').next().unwrap_or(path).to_string();
    let mut plan = Plan::default();
    match top.kind {
        Kind::File => {
            let size = top.size.unwrap_or(0);
            plan.items.push(Item { rel: name, kind: ItemKind::File, size });
            plan.total = size;
        }
        Kind::Dir => {
            plan.items.push(Item { rel: name.clone(), kind: ItemKind::Dir, size: 0 });
            walk(fs, path, &name, &mut plan)?;
        }
        _ => plan.skipped += 1,
    }
    Ok(plan)
}

fn walk(fs: &dyn FileSystem, dir: &str, rel: &str, plan: &mut Plan) -> Result<(), FsError> {
    let mut entries = fs.list(dir)?.entries;
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    for e in entries {
        let r = format!("{rel}/{}", e.name);
        match e.kind {
            Kind::Dir => {
                plan.items.push(Item { rel: r.clone(), kind: ItemKind::Dir, size: 0 });
                walk(fs, &join(dir, &e.name), &r, plan)?;
            }
            Kind::File => {
                let size = e.size.unwrap_or(0);
                plan.total += size;
                plan.items.push(Item { rel: r, kind: ItemKind::File, size });
            }
            _ => plan.skipped += 1,
        }
    }
    Ok(())
}

/// How much is written to the part at a time.
const WRITE_PIECE: usize = 1 << 20;

/// Copies one file to `dst_path` through `dst_path.part`: continuing from the part's size
/// with `resume`, from scratch otherwise. `on` hears the progress and can stop the copy
/// (see [`Flow`]). The part becomes the file by rename: `overwrite` replaces a file there,
/// else Exists (the part stays for a retry).
pub fn copy_file(
    src: &dyn FileSystem,
    src_path: &str,
    dst: &dyn FileSystem,
    dst_path: &str,
    resume: bool,
    overwrite: bool,
    on: &mut dyn FnMut(Progress) -> Flow,
) -> Result<u64, CopyError> {
    let part = part_path(dst_path);
    let stopped = |flow: Flow| {
        if flow == Flow::Abort {
            CopyError::Aborted
        } else {
            let _ = dst.remove(&part);
            CopyError::Canceled
        }
    };
    let total = src.stat_target(src_path).map_err(CopyError::Fs)?.size.unwrap_or(0);
    let mut start = if resume { dst.stat(&part).ok().and_then(|e| e.size).unwrap_or(0) } else { 0 };
    if !resume || start > total {
        match dst.remove(&part) {
            Ok(()) | Err(FsError::NotFound) => {}
            Err(e) => return Err(CopyError::Fs(e)),
        }
        start = 0;
    }
    if start == 0 {
        dst.write_chunk(&part, 0, &[]).map_err(CopyError::Fs)?;
    }
    match on(Progress { done: start, total }) {
        Flow::Go => {}
        f => return Err(stopped(f)),
    }
    let mut at = start;
    let mut buf: Vec<u8> = Vec::with_capacity(WRITE_PIECE);
    let mut failed: Option<FsError> = None;
    let mut halt = Flow::Go;
    let flush = |buf: &mut Vec<u8>, at: &mut u64| -> Result<(), FsError> {
        if !buf.is_empty() {
            dst.write_chunk(&part, *at, buf)?;
            *at += buf.len() as u64;
            buf.clear();
        }
        Ok(())
    };
    let read = src.read_stream(src_path, start, &mut |piece| {
        buf.extend_from_slice(piece);
        if buf.len() >= WRITE_PIECE {
            if let Err(e) = flush(&mut buf, &mut at) {
                failed = Some(e);
                return false;
            }
            halt = on(Progress { done: at, total });
            if halt != Flow::Go {
                return false;
            }
        }
        true
    });
    if let Err(e) = read {
        return Err(CopyError::Fs(e));
    }
    if halt != Flow::Go {
        return Err(stopped(halt));
    }
    if let Some(e) = failed {
        return Err(CopyError::Fs(e));
    }
    flush(&mut buf, &mut at).map_err(CopyError::Fs)?;
    match on(Progress { done: at, total }) {
        Flow::Go => {}
        f => return Err(stopped(f)),
    }
    let done = if overwrite {
        match dst.replace(&part, dst_path) {
            Err(FsError::Unsupported(_)) => {
                // Without an atomic replace: remove the old file first, never a folder.
                if dst.stat(dst_path).is_ok_and(|e| matches!(e.kind, Kind::Dir | Kind::Symlink { .. })) {
                    return Err(CopyError::Exists);
                }
                match dst.remove(dst_path) {
                    Ok(()) | Err(FsError::NotFound) => {}
                    Err(e) => return Err(CopyError::Fs(e)),
                }
                dst.rename(&part, dst_path)
            }
            other => other,
        }
    } else {
        dst.rename(&part, dst_path)
    };
    match done {
        Ok(()) => Ok(at),
        Err(FsError::Exists) => Err(CopyError::Exists),
        Err(e) => Err(CopyError::Fs(e)),
    }
}

/// Copies what `plan` lists (from `plan_tree(src, src_path)`) into `dst_dir`, the top item
/// named `dst_top` there (a "keep both" copy has another name). Folders are
/// made as needed; a file already there whole (same size, no part) is taken as done, so a
/// retry continues where the last run stopped.
pub fn copy_tree(
    src: &dyn FileSystem,
    src_path: &str,
    dst: &dyn FileSystem,
    dst_dir: &str,
    dst_top: &str,
    plan: &Plan,
    overwrite: bool,
    on: &mut dyn FnMut(Progress) -> Flow,
) -> Result<u64, CopyError> {
    let top = plan.items.first().map(|i| i.rel.clone()).unwrap_or_default();
    let src_base = crate::parent(src_path.trim_end_matches('/')).unwrap_or_else(|| "/".into());
    let mut done = 0u64;
    for item in &plan.items {
        let from = join(&src_base, &item.rel);
        let to = join(dst_dir, &format!("{dst_top}{}", &item.rel[top.len().min(item.rel.len())..]));
        match item.kind {
            ItemKind::Dir => match dst.create_dir(&to) {
                Ok(()) | Err(FsError::Exists) => {}
                Err(e) => return Err(CopyError::Fs(e)),
            },
            ItemKind::File => {
                let whole = dst.stat(&to).ok().is_some_and(|e| e.size == Some(item.size)) && dst.stat(&part_path(&to)).is_err();
                if whole && !overwrite {
                    done += item.size;
                    continue;
                }
                let base = done;
                let total = plan.total;
                copy_file(src, &from, dst, &to, true, overwrite, &mut |p| on(Progress { done: base + p.done, total }))?;
                done += item.size;
            }
        }
    }
    on(Progress { done, total: plan.total });
    Ok(done)
}

/// `name`, or `name (1)`, `name (2)`… before its extension, whichever isn't taken.
pub fn unique_name(exists: impl Fn(&str) -> bool, name: &str) -> String {
    if !exists(name) {
        return name.to_string();
    }
    // The extension starts at the first dot after the first character (".env" has none).
    let first = name.chars().next().map_or(0, char::len_utf8);
    let (stem, ext) = match name[first..].find('.') {
        Some(i) => name.split_at(i + first),
        None => (name, ""),
    };
    (1..)
        .map(|n| format!("{stem} ({n}){ext}"))
        .find(|c| !exists(c))
        .expect("some name is free")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::local::LocalFs;
    use std::fs;

    pub(crate) fn root(name: &str) -> String {
        let d = std::env::temp_dir().join(format!("rexa-xfer-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d.to_string_lossy().into_owned()
    }

    fn data(n: usize) -> Vec<u8> {
        (0..n as u32).map(|i| (i.wrapping_mul(2246822519) >> 11) as u8).collect()
    }

    /// The copy cases, from `src` to `dst` (both rooted in local folders).
    pub(crate) fn copy_cases(src: &dyn FileSystem, src_root: &str, dst: &dyn FileSystem, dst_root: &str) {
        // Odd names, a few MB, progress reported, no part left.
        let bytes = data(3_000_123);
        let s = format!("{src_root}/-it's 中文.bin");
        fs::write(&s, &bytes).unwrap();
        let d = format!("{dst_root}/-it's 中文.bin");
        let mut last = Progress { done: 0, total: 0 };
        assert_eq!(copy_file(src, &s, dst, &d, false, false, &mut |p| {
            last = p;
            Flow::Go
        }), Ok(bytes.len() as u64));
        assert_eq!(fs::read(&d).unwrap(), bytes);
        assert_eq!(last, Progress { done: bytes.len() as u64, total: bytes.len() as u64 });
        assert!(!std::path::Path::new(&part_path(&d)).exists());

        // An empty file.
        fs::write(format!("{src_root}/empty"), b"").unwrap();
        copy_file(src, &format!("{src_root}/empty"), dst, &format!("{dst_root}/empty"), false, false, &mut |_| Flow::Go).unwrap();
        assert_eq!(fs::read(format!("{dst_root}/empty")).unwrap(), b"");

        // Resume from half a part: byte-identical, nothing written twice.
        let r = format!("{dst_root}/resumed.bin");
        fs::write(part_path(&r), &bytes[..1_234_567]).unwrap();
        let mut first = None;
        copy_file(src, &s, dst, &r, true, false, &mut |p| {
            first.get_or_insert(p.done);
            Flow::Go
        })
        .unwrap();
        assert_eq!(fs::read(&r).unwrap(), bytes);
        assert!(first.unwrap() >= 1_234_567);

        // Canceled: no file, no part.
        let c = format!("{dst_root}/canceled.bin");
        assert_eq!(copy_file(src, &s, dst, &c, false, false, &mut |p| if p.done < 1_000_000 { Flow::Go } else { Flow::Cancel }), Err(CopyError::Canceled));
        assert!(!std::path::Path::new(&c).exists() && !std::path::Path::new(&part_path(&c)).exists());

        // The target appeared: refused, the part kept for a retry; overwrite replaces it.
        let t = format!("{dst_root}/taken.bin");
        fs::write(&t, b"theirs").unwrap();
        assert_eq!(copy_file(src, &s, dst, &t, false, false, &mut |_| Flow::Go), Err(CopyError::Exists));
        assert_eq!(fs::read(&t).unwrap(), b"theirs");
        assert!(std::path::Path::new(&part_path(&t)).exists());
        copy_file(src, &s, dst, &t, true, true, &mut |_| Flow::Go).unwrap();
        assert_eq!(fs::read(&t).unwrap(), bytes);

        // A tree: nested and empty folders kept, a link skipped.
        let tree = format!("{src_root}/proj dir");
        fs::create_dir_all(format!("{tree}/src/deep")).unwrap();
        fs::create_dir_all(format!("{tree}/empty")).unwrap();
        fs::write(format!("{tree}/src/deep/a.rs"), b"fn a() {}").unwrap();
        fs::write(format!("{tree}/.env"), b"K=1").unwrap();
        std::os::unix::fs::symlink("/etc", format!("{tree}/etc-link")).unwrap();
        let plan = plan_tree(src, &tree).unwrap();
        assert_eq!(plan.skipped, 1);
        assert_eq!(plan.total, 12);
        copy_tree(src, &tree, dst, dst_root, "proj dir", &plan, false, &mut |_| Flow::Go).unwrap();
        assert_eq!(fs::read(format!("{dst_root}/proj dir/src/deep/a.rs")).unwrap(), b"fn a() {}");
        assert_eq!(fs::read(format!("{dst_root}/proj dir/.env")).unwrap(), b"K=1");
        assert!(std::path::Path::new(&format!("{dst_root}/proj dir/empty")).is_dir());
        assert!(!std::path::Path::new(&format!("{dst_root}/proj dir/etc-link")).exists());
        // Kept both: the same tree under another name.
        copy_tree(src, &tree, dst, dst_root, "proj dir (1)", &plan, false, &mut |_| Flow::Go).unwrap();
        assert_eq!(fs::read(format!("{dst_root}/proj dir (1)/src/deep/a.rs")).unwrap(), b"fn a() {}");
        // Again (a retry): what's there is left alone, nothing fails.
        copy_tree(src, &tree, dst, dst_root, "proj dir", &plan, false, &mut |_| Flow::Go).unwrap();
    }

    #[test]
    fn local_copy_cases() {
        copy_cases(&LocalFs, &root("src"), &LocalFs, &root("dst"));
    }

    #[test]
    fn a_link_at_the_top_is_followed() {
        let r = root("toplink");
        let d = root("toplink-dst");
        fs::write(format!("{r}/app.tar"), b"payload").unwrap();
        std::os::unix::fs::symlink(format!("{r}/app.tar"), format!("{r}/current")).unwrap();
        let plan = plan_tree(&LocalFs, &format!("{r}/current")).unwrap();
        assert_eq!(plan.items.len(), 1);
        assert_eq!(plan.items[0].kind, ItemKind::File);
        copy_file(&LocalFs, &format!("{r}/current"), &LocalFs, &format!("{d}/current"), false, false, &mut |_| Flow::Go).unwrap();
        assert_eq!(fs::read(format!("{d}/current")).unwrap(), b"payload");
    }

    #[test]
    fn overwrite_without_atomic_rename_never_removes_a_folder() {
        let r = root("nofold");
        let d = root("nofold-dst");
        fs::write(format!("{r}/build"), b"file").unwrap();
        fs::create_dir(format!("{d}/build")).unwrap();
        fs::write(format!("{d}/build/keep.o"), b"keep").unwrap();
        let dst = crate::save::tests::Twist::replace_fails(FsError::Unsupported("no".into()));
        assert!(copy_file(&LocalFs, &format!("{r}/build"), &dst, &format!("{d}/build"), false, true, &mut |_| Flow::Go).is_err());
        assert_eq!(fs::read(format!("{d}/build/keep.o")).unwrap(), b"keep");
    }

    #[test]
    fn abort_keeps_the_part() {
        let r = root("abort");
        let d = root("abort-dst");
        fs::write(format!("{r}/f"), data(3_000_000)).unwrap();
        let to = format!("{d}/f");
        assert_eq!(copy_file(&LocalFs, &format!("{r}/f"), &LocalFs, &to, false, false, &mut |p| if p.done < 1_000_000 { Flow::Go } else { Flow::Abort }), Err(CopyError::Aborted));
        assert!(fs::metadata(part_path(&to)).unwrap().len() >= 1_000_000);
        copy_file(&LocalFs, &format!("{r}/f"), &LocalFs, &to, true, false, &mut |_| Flow::Go).unwrap();
        assert_eq!(fs::read(&to).unwrap(), data(3_000_000));
    }

    #[test]
    fn unique_names() {
        assert_eq!(unique_name(|n| n == "中文.txt", "中文.txt"), "中文 (1).txt");
        assert_eq!(unique_name(|n| n == "é", "é"), "é (1)");
        let taken = ["a.txt", "a (1).txt", ".env", "archive.tar.gz"];
        let exists = |n: &str| taken.contains(&n);
        assert_eq!(unique_name(exists, "a.txt"), "a (2).txt");
        assert_eq!(unique_name(exists, ".env"), ".env (1)");
        assert_eq!(unique_name(exists, "archive.tar.gz"), "archive (1).tar.gz");
        assert_eq!(unique_name(exists, "free.txt"), "free.txt");
        assert_eq!(unique_name(exists, "noext"), "noext");
    }
}
