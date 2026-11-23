//! SFTP version 3 packets (draft-ietf-secsh-filexfer-02): what we send and what we read.

pub const INIT: u8 = 1;
pub const VERSION: u8 = 2;
pub const OPEN: u8 = 3;
pub const CLOSE: u8 = 4;
pub const READ: u8 = 5;
pub const LSTAT: u8 = 7;
pub const OPENDIR: u8 = 11;
pub const READDIR: u8 = 12;
pub const REMOVE: u8 = 13;
pub const MKDIR: u8 = 14;
pub const RMDIR: u8 = 15;
pub const REALPATH: u8 = 16;
pub const STAT: u8 = 17;
pub const RENAME: u8 = 18;
pub const EXTENDED: u8 = 200;

pub const STATUS: u8 = 101;
pub const HANDLE: u8 = 102;
pub const DATA: u8 = 103;
pub const NAME: u8 = 104;
pub const ATTRS: u8 = 105;

pub const FX_OK: u32 = 0;
pub const FX_EOF: u32 = 1;
pub const FX_NO_SUCH_FILE: u32 = 2;
pub const FX_PERMISSION_DENIED: u32 = 3;

pub const READ_FLAG: u32 = 0x01;
pub const WRITE_FLAG: u32 = 0x02;
pub const CREAT: u32 = 0x08;
pub const EXCL: u32 = 0x20;

const ATTR_SIZE: u32 = 0x01;
const ATTR_UIDGID: u32 = 0x02;
const ATTR_PERMISSIONS: u32 = 0x04;
const ATTR_ACMODTIME: u32 = 0x08;
const ATTR_EXTENDED: u32 = 0x8000_0000;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Attrs {
    pub size: Option<u64>,
    pub uid_gid: Option<(u32, u32)>,
    pub permissions: Option<u32>,
    pub times: Option<(u32, u32)>,
}

/// A packet being built: type, then fields; `finish` puts the length in front.
pub struct Out(Vec<u8>);

impl Out {
    pub fn new(kind: u8) -> Self {
        let mut v = vec![0, 0, 0, 0];
        v.push(kind);
        Out(v)
    }
    pub fn u32(mut self, n: u32) -> Self {
        self.0.extend(n.to_be_bytes());
        self
    }
    pub fn u64(mut self, n: u64) -> Self {
        self.0.extend(n.to_be_bytes());
        self
    }
    pub fn bytes(mut self, b: &[u8]) -> Self {
        self.0.extend((b.len() as u32).to_be_bytes());
        self.0.extend(b);
        self
    }
    pub fn str(self, s: &str) -> Self {
        self.bytes(s.as_bytes())
    }
    pub fn attrs(self, a: &Attrs) -> Self {
        let mut flags = 0;
        if a.size.is_some() {
            flags |= ATTR_SIZE;
        }
        if a.uid_gid.is_some() {
            flags |= ATTR_UIDGID;
        }
        if a.permissions.is_some() {
            flags |= ATTR_PERMISSIONS;
        }
        if a.times.is_some() {
            flags |= ATTR_ACMODTIME;
        }
        let mut o = self.u32(flags);
        if let Some(s) = a.size {
            o = o.u64(s);
        }
        if let Some((u, g)) = a.uid_gid {
            o = o.u32(u).u32(g);
        }
        if let Some(p) = a.permissions {
            o = o.u32(p);
        }
        if let Some((at, mt)) = a.times {
            o = o.u32(at).u32(mt);
        }
        o
    }
    pub fn finish(mut self) -> Vec<u8> {
        let len = (self.0.len() - 4) as u32;
        self.0[..4].copy_from_slice(&len.to_be_bytes());
        self.0
    }
}

/// A packet's body being read.
pub struct In<'a> {
    buf: &'a [u8],
}

#[derive(Debug, PartialEq, Eq)]
pub struct Short;

impl<'a> In<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        In { buf }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], Short> {
        if self.buf.len() < n {
            return Err(Short);
        }
        let (a, b) = self.buf.split_at(n);
        self.buf = b;
        Ok(a)
    }
    pub fn u8(&mut self) -> Result<u8, Short> {
        Ok(self.take(1)?[0])
    }
    pub fn u32(&mut self) -> Result<u32, Short> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }
    pub fn u64(&mut self) -> Result<u64, Short> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }
    pub fn bytes(&mut self) -> Result<&'a [u8], Short> {
        let n = self.u32()? as usize;
        self.take(n)
    }
    pub fn string(&mut self) -> Result<String, Short> {
        Ok(String::from_utf8_lossy(self.bytes()?).into_owned())
    }
    pub fn attrs(&mut self) -> Result<Attrs, Short> {
        let flags = self.u32()?;
        let mut a = Attrs::default();
        if flags & ATTR_SIZE != 0 {
            a.size = Some(self.u64()?);
        }
        if flags & ATTR_UIDGID != 0 {
            a.uid_gid = Some((self.u32()?, self.u32()?));
        }
        if flags & ATTR_PERMISSIONS != 0 {
            a.permissions = Some(self.u32()?);
        }
        if flags & ATTR_ACMODTIME != 0 {
            a.times = Some((self.u32()?, self.u32()?));
        }
        if flags & ATTR_EXTENDED != 0 {
            for _ in 0..self.u32()? {
                self.bytes()?;
                self.bytes()?;
            }
        }
        Ok(a)
    }
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_round_trip() {
        let a = Attrs { size: Some(1 << 40), uid_gid: Some((1000, 100)), permissions: Some(0o100644), times: Some((1, 2)) };
        let p = Out::new(OPEN).u32(7).str("dir/中文 it's").u32(READ_FLAG | CREAT).attrs(&a).finish();
        assert_eq!(u32::from_be_bytes(p[..4].try_into().unwrap()) as usize, p.len() - 4);
        let mut r = In::new(&p[4..]);
        assert_eq!(r.u8(), Ok(OPEN));
        assert_eq!(r.u32(), Ok(7));
        assert_eq!(r.string(), Ok("dir/中文 it's".to_string()));
        assert_eq!(r.u32(), Ok(READ_FLAG | CREAT));
        assert_eq!(r.attrs(), Ok(a));
        assert!(r.is_empty());
        assert_eq!(r.u32(), Err(Short));
        // Empty attrs are just the flags word.
        let e = Out::new(MKDIR).attrs(&Attrs::default()).finish();
        assert_eq!(&e[5..], &[0, 0, 0, 0]);
        // Extended attributes are skipped.
        let mut x = 0x8000_0001u32.to_be_bytes().to_vec();
        x.extend(5u64.to_be_bytes());
        x.extend(1u32.to_be_bytes());
        for s in ["k", "v"] {
            x.extend((s.len() as u32).to_be_bytes());
            x.extend(s.as_bytes());
        }
        assert_eq!(In::new(&x).attrs(), Ok(Attrs { size: Some(5), ..Default::default() }));
    }
}
