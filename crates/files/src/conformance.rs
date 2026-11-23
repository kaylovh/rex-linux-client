//! One scenario every FileSystem must pass, with awkward names.

use crate::{FileSystem, FsError, Kind, join};

/// `seed(path, bytes)` writes a file and `link(target, path)` makes a symlink, both outside
/// the file system under test (the root is a local directory in every test).
pub fn run(fs: &dyn FileSystem, root: &str, seed: &dyn Fn(&str, &[u8]), link: &dyn Fn(&str, &str)) {
    let names = ["a b", "it's", "中文.txt", "-dash"];
    for n in names {
        fs.create_file(&join(root, n)).unwrap_or_else(|e| panic!("create {n}: {e}"));
    }
    assert_eq!(fs.create_file(&join(root, "a b")), Err(FsError::Exists));
    let sub = join(root, "sub dir");
    fs.create_dir(&sub).unwrap();
    assert_eq!(fs.create_dir(&sub), Err(FsError::Exists));
    fs.create_file(&join(&sub, "inner")).unwrap();
    seed(&join(root, "hello.txt"), b"hello world");
    link(&sub, &join(root, "to-sub"));

    let listing = fs.list(root).unwrap();
    assert!(listing.path.starts_with('/'), "{}", listing.path);
    let kind = |n: &str| listing.entries.iter().find(|e| e.name == n).map(|e| e.kind);
    for n in names {
        assert_eq!(kind(n), Some(Kind::File), "{n}");
    }
    assert_eq!(kind("sub dir"), Some(Kind::Dir));
    assert_eq!(kind("to-sub"), Some(Kind::Symlink { to_dir: true }));
    assert!(!listing.entries.iter().any(|e| e.name == "." || e.name == ".."));

    let hello = fs.stat(&join(root, "hello.txt")).unwrap();
    assert_eq!((hello.kind, hello.size), (Kind::File, Some(11)));
    assert_eq!(fs.read(&join(root, "hello.txt"), 5).unwrap(), b"hello");
    assert_eq!(fs.read(&join(root, "hello.txt"), 1 << 20).unwrap(), b"hello world");

    fs.rename(&join(root, "a b"), &join(root, "renamed")).unwrap();
    assert_eq!(fs.rename(&join(root, "it's"), &join(root, "renamed")), Err(FsError::Exists));
    assert_eq!(fs.stat(&join(root, "a b")), Err(FsError::NotFound));

    // The link goes, not what it points at; then the folder with its contents.
    fs.remove(&join(root, "to-sub")).unwrap();
    assert_eq!(fs.stat(&join(&sub, "inner")).map(|e| e.kind), Ok(Kind::File));
    fs.remove(&sub).unwrap();
    assert_eq!(fs.stat(&sub), Err(FsError::NotFound));

    assert_eq!(fs.list(&join(root, "nope")), Err(FsError::NotFound));
    assert_eq!(fs.list(&join(root, "hello.txt")), Err(FsError::NotDir));
    assert!(fs.home().unwrap().starts_with('/'));
    assert_eq!(fs.writable(), Ok(()));
}
