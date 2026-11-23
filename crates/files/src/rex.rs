//! Files on a machine Rex reaches but ssh doesn't: listed and read through a terminal there
//! (`list_dir`, `download_begin`). Rex can't change files, so this is read-only.

use std::sync::Arc;

use rex_proto::control::Control;
use rex_proto::data::{BYTES_V1, DataConn, DataItem};
use rex_proto::transport::Endpoint;
use serde_json::{Value, json};

use crate::{FileEntry, FileSystem, FsError, Kind, Listing, parent};

/// Why writes fail here (English; the app shows its own words).
pub const READ_ONLY: &str = "files on this machine are reached through Rex, which can only read them";

pub struct RexFs {
    endpoint: Endpoint,
    control: Arc<Control>,
    session: String,
    block: String,
}

impl RexFs {
    pub fn new(endpoint: Endpoint, control: Arc<Control>, session: String, block: String) -> Self {
        RexFs { endpoint, control, session, block }
    }

    fn call(&self, method: &str, args: Value) -> Result<Value, FsError> {
        self.control
            .call(method, json!({"session_id": self.session, "block_id": self.block, "args": args}))
            .map_err(|e| FsError::Other(e.to_string()))
    }

    fn list_dir(&self, path: &str, prefix: Option<&str>) -> Result<Value, FsError> {
        let mut args = json!({"path": path, "files": true});
        if let Some(p) = prefix {
            args["prefix"] = json!(p);
        }
        let v = self.call("com.superlogical.terminal.list_dir", args)?;
        match v["status"].as_str() {
            Some("ok") => Ok(v),
            Some("missing") => Err(FsError::NotFound),
            Some("not_dir") => Err(FsError::NotDir),
            Some("denied") => Err(FsError::Denied),
            other => Err(FsError::Other(format!("list_dir: {other:?}"))),
        }
    }
}

fn entry(e: &Value) -> Option<FileEntry> {
    let symlink = e["symlink"] == true;
    let file = e["file"] == true;
    let kind = match (symlink, file) {
        (true, f) => Kind::Symlink { to_dir: !f },
        (false, true) => Kind::File,
        (false, false) => Kind::Dir,
    };
    Some(FileEntry { name: e["name"].as_str()?.to_string(), kind, size: None, modified: None, mode: None, uid: None, gid: None, nlink: None })
}

/// A download reference: "download:" and a random version-4 UUID.
fn download_ref() -> String {
    let mut b = [0u8; 16];
    let _ = getrandom::fill(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("download:{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

fn read_only<T>() -> Result<T, FsError> {
    Err(FsError::ReadOnly(READ_ONLY.into()))
}

impl FileSystem for RexFs {
    fn home(&self) -> Result<String, FsError> {
        let v = self.list_dir("~", Some("\u{0}"))?;
        v["home"].as_str().filter(|h| !h.is_empty()).or(v["path"].as_str()).map(str::to_string).ok_or(FsError::NotFound)
    }

    fn list(&self, path: &str) -> Result<Listing, FsError> {
        let v = self.list_dir(path, None)?;
        Ok(Listing {
            path: v["path"].as_str().unwrap_or(path).to_string(),
            entries: v["entries"].as_array().into_iter().flatten().filter_map(entry).collect(),
            truncated: v["truncated"] == true,
        })
    }

    fn stat(&self, path: &str) -> Result<FileEntry, FsError> {
        let trimmed = path.trim_end_matches('/');
        let (Some(dir), Some(name)) = (parent(trimmed), trimmed.rsplit('/').next()) else {
            return Ok(FileEntry { name: "/".into(), kind: Kind::Dir, size: None, modified: None, mode: None, uid: None, gid: None, nlink: None });
        };
        let v = self.list_dir(&dir, Some(name))?;
        v["entries"].as_array().into_iter().flatten().filter_map(entry).find(|e| e.name == name).ok_or(FsError::NotFound)
    }

    fn stat_target(&self, path: &str) -> Result<FileEntry, FsError> {
        // Rex marks anything that isn't a folder as a file; download_begin refuses
        // anything that isn't a regular file, so nothing can block there.
        let mut e = self.stat(path)?;
        if let Kind::Symlink { to_dir } = e.kind {
            e.kind = if to_dir { Kind::Dir } else { Kind::File };
        }
        Ok(e)
    }

    fn read(&self, path: &str, max: u64) -> Result<Vec<u8>, FsError> {
        let v = self.call("com.superlogical.terminal.download_begin", json!({"ref": download_ref(), "path": path}))?;
        let ticket = v["ticket"].as_str().ok_or_else(|| FsError::Other("no download ticket".into()))?;
        let mut conn = DataConn::open(&self.endpoint, ticket, BYTES_V1).map_err(|e| FsError::Gone(e.to_string()))?;
        let mut out = Vec::new();
        while (out.len() as u64) < max {
            match conn.next().map_err(|e| FsError::Gone(e.to_string()))? {
                DataItem::Bytes(b) => out.extend_from_slice(&b),
                DataItem::Closed(_) => break,
            }
        }
        out.truncate(max.min(out.len() as u64) as usize);
        conn.close();
        Ok(out)
    }

    fn create_file(&self, _path: &str) -> Result<(), FsError> {
        read_only()
    }
    fn create_dir(&self, _path: &str) -> Result<(), FsError> {
        read_only()
    }
    fn rename(&self, _from: &str, _to: &str) -> Result<(), FsError> {
        read_only()
    }
    fn remove(&self, _path: &str) -> Result<(), FsError> {
        read_only()
    }
    fn writable(&self) -> Result<(), String> {
        Err(READ_ONLY.into())
    }
    fn resolve(&self, _path: &str) -> Result<String, FsError> {
        read_only()
    }
    fn write_file(&self, _path: &str, _bytes: &[u8], _create_new: bool) -> Result<(), FsError> {
        read_only()
    }
    fn set_mode(&self, _path: &str, _mode: u32) -> Result<(), FsError> {
        read_only()
    }
    fn replace(&self, _from: &str, _to: &str) -> Result<(), FsError> {
        read_only()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rex_proto::testing::TestServer;
    use serde_json::json;

    #[test]
    fn rex_fs_lists_and_reads() {
        let Some(srv) = TestServer::start() else { return };
        let ctl = Control::connect(srv.endpoint.clone(), "rexa-desktop-test").unwrap();
        let created = ctl
            .call("session.create", json!({"initial_windows": [{"layout": {"block": {"flavor": "com.superlogical.terminal.shell"}}}]}))
            .unwrap();
        let sid = created["session_id"].as_str().unwrap().to_string();
        let bid = created["initial_windows"][0]["block_ids"][0].as_str().unwrap().to_string();
        ctl.call("session.attach", json!({"session_id": sid})).unwrap();

        let dir = std::env::temp_dir().join(format!("rexa-rexfs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub dir")).unwrap();
        std::fs::write(dir.join("it's.txt"), b"hello through rex").unwrap();
        std::os::unix::fs::symlink(dir.join("sub dir"), dir.join("link")).unwrap();
        let root = dir.to_string_lossy().into_owned();

        let fs = RexFs::new(srv.endpoint.clone(), ctl.clone(), sid, bid);
        let listing = fs.list(&root).unwrap();
        let kind = |n: &str| listing.entries.iter().find(|e| e.name == n).map(|e| e.kind);
        assert_eq!(kind("sub dir"), Some(Kind::Dir));
        assert_eq!(kind("it's.txt"), Some(Kind::File));
        assert_eq!(kind("link"), Some(Kind::Symlink { to_dir: true }));
        assert_eq!(fs.stat(&format!("{root}/it's.txt")).map(|e| e.kind), Ok(Kind::File));
        assert_eq!(fs.read(&format!("{root}/it's.txt"), 5).unwrap(), b"hello");
        assert_eq!(fs.read(&format!("{root}/it's.txt"), 1 << 20).unwrap(), b"hello through rex");
        assert_eq!(fs.list(&format!("{root}/nope")), Err(FsError::NotFound));
        assert!(fs.home().unwrap().starts_with('/'));
        assert!(fs.writable().is_err());
        assert!(matches!(fs.create_file(&format!("{root}/x")), Err(FsError::ReadOnly(_))));
        assert!(matches!(fs.remove(&format!("{root}/it's.txt")), Err(FsError::ReadOnly(_))));
        assert!(dir.join("it's.txt").exists());
        ctl.close();
    }
}
