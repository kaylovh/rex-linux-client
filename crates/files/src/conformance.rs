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

    // What a path is once links are followed; a FIFO is not a file to read.
    assert_eq!(fs.stat_target(&join(root, "hello.txt")).map(|e| e.kind), Ok(Kind::File));
    let fifo = join(root, "pipe");
    assert!(std::process::Command::new("mkfifo").arg(&fifo).status().unwrap().success());
    assert_eq!(fs.stat_target(&fifo).map(|e| e.kind), Ok(Kind::Other));
    link(&join(root, "hello.txt"), &join(root, "to-hello"));
    assert_eq!(fs.stat_target(&join(root, "to-hello")).map(|e| e.kind), Ok(Kind::File));

    assert_eq!(fs.list(&join(root, "nope")), Err(FsError::NotFound));
    assert_eq!(fs.list(&join(root, "hello.txt")), Err(FsError::NotDir));
    assert!(fs.home().unwrap().starts_with('/'));
    assert_eq!(fs.writable(), Ok(()));
}

/// Writing: new files, truncating overwrites, modes, atomic replace, resolving links.
pub fn run_write(fs: &dyn FileSystem, root: &str, seed: &dyn Fn(&str, &[u8]), link: &dyn Fn(&str, &str)) {
    let f = join(root, "w it's.txt");
    fs.write_file(&f, b"hello world", true, None).unwrap();
    assert_eq!(fs.write_file(&f, b"x", true, None), Err(FsError::Exists));
    fs.write_file(&f, b"abc", false, None).unwrap();
    assert_eq!(fs.read(&f, 1 << 20).unwrap(), b"abc");

    fs.set_mode(&f, 0o600).unwrap();
    assert_eq!(fs.stat(&f).unwrap().mode.map(|m| m & 0o777), Some(0o600));

    // Bigger than one SFTP packet.
    let big: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    let b = join(root, "big.bin");
    fs.write_file(&b, &big, true, None).unwrap();
    assert_eq!(fs.read(&b, 1 << 20).unwrap(), big);

    seed(&join(root, "old"), b"old");
    seed(&join(root, "new"), b"new");
    fs.replace(&join(root, "new"), &join(root, "old")).unwrap();
    assert_eq!(fs.read(&join(root, "old"), 100).unwrap(), b"new");
    assert_eq!(fs.stat(&join(root, "new")), Err(FsError::NotFound));

    // Links: absolute, relative, and a link to a link all resolve to the file.
    let real = fs.resolve(&f).unwrap();
    link(&f, &join(root, "abs-link"));
    link("w it's.txt", &join(root, "rel-link"));
    link(&join(root, "rel-link"), &join(root, "link-link"));
    for l in ["abs-link", "rel-link", "link-link"] {
        assert_eq!(fs.resolve(&join(root, l)).unwrap(), real, "{l}");
    }
    assert_eq!(fs.resolve(&join(root, "missing")), Err(FsError::NotFound));
    let e = fs.stat(&f).unwrap();
    assert!(e.uid.is_some() && e.gid.is_some());

    // A file can be created private from its first byte.
    let p = join(root, "private");
    fs.write_file(&p, b"secret", true, Some(0o600)).unwrap();
    assert_eq!(fs.stat(&p).unwrap().mode.map(|m| m & 0o777), Some(0o600));
    // Link counts, where the source knows them.
    std::fs::hard_link(&p, join(root, "private2")).unwrap();
    assert_eq!(fs.link_count(&p).unwrap(), Some(2));
    assert_eq!(fs.link_count(&b).unwrap(), Some(1));
}

/// Chunked writes and reads: offsets, a 5 MB file, streaming from an offset, stopping a stream.
pub fn run_chunks(fs: &dyn FileSystem, root: &str) {
    let p = join(root, "chunked it's.bin");
    let data: Vec<u8> = (0..5_000_000u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
    for (i, part) in data.chunks(1 << 20).enumerate() {
        fs.write_chunk(&p, (i << 20) as u64, part).unwrap();
    }
    assert_eq!(fs.stat(&p).unwrap().size, Some(data.len() as u64));
    assert_eq!(fs.read_chunk(&p, 123_457, 99_999).unwrap(), &data[123_457..123_457 + 99_999]);
    assert!(fs.read_chunk(&p, data.len() as u64, 10).unwrap().is_empty());
    let mut got = Vec::new();
    assert!(fs.read_stream(&p, 1_000_001, &mut |b| {
        got.extend_from_slice(b);
        true
    })
    .unwrap());
    assert_eq!(got, &data[1_000_001..]);
    let mut seen = 0usize;
    assert!(!fs.read_stream(&p, 0, &mut |b| {
        seen += b.len();
        seen < 2_000_000
    })
    .unwrap());
    // A chunk past the end extends; a chunk inside never truncates.
    fs.write_chunk(&p, 0, b"XY").unwrap();
    assert_eq!(fs.stat(&p).unwrap().size, Some(data.len() as u64));
    assert_eq!(fs.read_chunk(&p, 0, 3).unwrap(), [b'X', b'Y', data[2]]);
}
