//! Append-only binary logs published for the web UI.
//!
//! Published index files are never rewritten in place: the bot only appends
//! records, and the UI fetches just the new bytes with an HTTP Range request
//! (`Range: bytes=<known length>-`). A file is:
//!
//! ```text
//! "MAL1"                         magic
//! record*                        varint body_len, then body = type byte + fields
//! ```
//!
//! The first record is always a [`HEADER`]. Integers are LEB128 varints
//! (signed values zigzag-encoded), strings and byte blobs are varint length +
//! bytes. A reader parses only complete records, so it never sees a record
//! that is still being written; a crash can only leave an incomplete tail,
//! which the writer truncates when it reopens the file. Readers ignore
//! unknown record types, so new types can be added compatibly.

use anyhow::{bail, Context, Result};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

pub const MAGIC: &[u8; 4] = b"MAL1";

// Record types (one namespace for all files).
pub const HEADER: u8 = 0;
/// days.log: a day exists (key, from, to).
pub const DAY: u8 = 1;
/// days.log: totals of a finished day; a later summary of the same day replaces it.
pub const SUMMARY: u8 = 2;
/// days.log: a day was removed by retention.
pub const DAY_DELETED: u8 = 3;
/// channels log / day log: channel definition (latest wins).
pub const CHANNEL: u8 = 4;
/// channels log: channel no longer exists.
pub const CHANNEL_REMOVE: u8 = 5;
/// channels log: this generation is superseded by a compacted one.
pub const NEXT: u8 = 6;
/// day log: speaker definition (latest wins).
pub const SPEAKER: u8 = 7;
pub const SEGMENT: u8 = 8;
pub const PRESENCE_OPEN: u8 = 9;
pub const PRESENCE_CLOSE: u8 = 10;
pub const UPTIME_OPEN: u8 = 11;
pub const UPTIME_CLOSE: u8 = 12;

// HEADER kinds.
pub const KIND_DAYS: u8 = 1;
pub const KIND_CHANNELS: u8 = 2;
pub const KIND_DAY: u8 = 3;

// ---------- encoding ----------

#[derive(Default)]
pub struct Rec(Vec<u8>);

impl Rec {
    pub fn new(ty: u8) -> Self {
        Self(vec![ty])
    }
    pub fn u(mut self, mut v: u64) -> Self {
        loop {
            let b = (v & 0x7F) as u8;
            v >>= 7;
            if v == 0 {
                self.0.push(b);
                return self;
            }
            self.0.push(b | 0x80);
        }
    }
    /// Signed (zigzag).
    pub fn i(self, v: i64) -> Self {
        self.u(((v << 1) ^ (v >> 63)) as u64)
    }
    /// Optional non-negative id: 0 = none, else id + 1.
    pub fn opt(self, v: Option<i64>) -> Self {
        self.u(v.map_or(0, |x| x.max(0) as u64 + 1))
    }
    pub fn bytes(self, b: &[u8]) -> Self {
        let mut s = self.u(b.len() as u64);
        s.0.extend_from_slice(b);
        s
    }
    pub fn str(self, s: &str) -> Self {
        self.bytes(s.as_bytes())
    }
    /// Framed record: varint length + body.
    pub fn frame(self, out: &mut Vec<u8>) {
        let mut n = self.0.len() as u64;
        loop {
            let b = (n & 0x7F) as u8;
            n >>= 7;
            if n == 0 {
                out.push(b);
                break;
            }
            out.push(b | 0x80);
        }
        out.extend_from_slice(&self.0);
    }
}

// ---------- decoding ----------

pub struct Fields<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Fields<'a> {
    pub fn u(&mut self) -> Result<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let Some(&b) = self.buf.get(self.pos) else { bail!("truncated field") };
            self.pos += 1;
            v |= ((b & 0x7F) as u64) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        bail!("bad varint")
    }
    pub fn i(&mut self) -> Result<i64> {
        let v = self.u()?;
        Ok((v >> 1) as i64 ^ -((v & 1) as i64))
    }
    pub fn opt(&mut self) -> Result<Option<i64>> {
        Ok(match self.u()? {
            0 => None,
            v => Some(v as i64 - 1),
        })
    }
    pub fn bytes(&mut self) -> Result<&'a [u8]> {
        let n = self.u()? as usize;
        let b = self.buf.get(self.pos..self.pos + n).context("truncated bytes")?;
        self.pos += n;
        Ok(b)
    }
    pub fn str(&mut self) -> Result<String> {
        Ok(String::from_utf8_lossy(self.bytes()?).into_owned())
    }
}

/// Complete records of a log (type, fields), and the length of the valid prefix.
pub fn parse(data: &[u8]) -> Result<(Vec<(u8, Fields<'_>)>, usize)> {
    if data.len() < 4 || &data[..4] != MAGIC {
        bail!("not a published log");
    }
    let mut out = vec![];
    let mut pos = 4;
    loop {
        let mut f = Fields { buf: data, pos };
        let Ok(len) = f.u() else { break };
        let start = f.pos;
        let end = start + len as usize;
        if len == 0 || end > data.len() {
            break;
        }
        out.push((data[start], Fields { buf: &data[..end], pos: start + 1 }));
        pos = end;
    }
    Ok((out, pos))
}

// ---------- files ----------

/// An append-only log file. Writes are buffered per call and appended with a
/// single write so readers see whole records as soon as possible.
pub struct LogFile {
    pub path: PathBuf,
    file: File,
    pub len: u64,
}

impl LogFile {
    /// Create a new log with a header record (replacing any existing file).
    pub fn create(path: &Path, header: Rec) -> Result<Self> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let mut buf = MAGIC.to_vec();
        header.frame(&mut buf);
        // Write to a temp file and rename so readers never see a half-made header.
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, &buf)?;
        std::fs::rename(&tmp, path)?;
        Self::open_existing(path)
    }

    fn open_existing(path: &Path) -> Result<Self> {
        let file = OpenOptions::new().read(true).append(true).open(path).with_context(|| format!("open {}", path.display()))?;
        let len = file.metadata()?.len();
        Ok(Self { path: path.to_path_buf(), file, len })
    }

    /// Open an existing log, dropping an incomplete tail left by a crash.
    /// Returns the file and its full content (valid part).
    pub fn open(path: &Path) -> Result<(Self, Vec<u8>)> {
        let mut data = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
        let (_, valid) = parse(&data).with_context(|| path.display().to_string())?;
        if valid < data.len() {
            tracing::warn!("{}: dropping {} bytes of incomplete record", path.display(), data.len() - valid);
            // (an append-only handle may not truncate on every platform)
            OpenOptions::new().write(true).open(path)?.set_len(valid as u64)?;
            data.truncate(valid);
        }
        Ok((Self::open_existing(path)?, data))
    }

    pub fn append(&mut self, recs: Vec<Rec>) -> Result<()> {
        if recs.is_empty() {
            return Ok(());
        }
        let mut buf = vec![];
        for r in recs {
            r.frame(&mut buf);
        }
        self.file.write_all(&buf)?;
        self.len += buf.len() as u64;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_torn_tail() {
        let dir = std::env::temp_dir().join(format!("publog-test-{}", std::process::id()));
        let path = dir.join("x.log");
        let mut f = LogFile::create(&path, Rec::new(HEADER).u(KIND_DAYS as u64)).unwrap();
        f.append(vec![Rec::new(DAY).str("2026-10-12").u(1_791_763_200_000).i(-5).opt(None).opt(Some(7))]).unwrap();
        drop(f);
        // simulate a crash mid-record
        let mut raw = std::fs::read(&path).unwrap();
        let good = raw.len();
        raw.extend_from_slice(&[40, SEGMENT, 1, 2]);
        std::fs::write(&path, &raw).unwrap();

        let (f, data) = LogFile::open(&path).unwrap();
        assert_eq!(f.len as usize, good);
        let (recs, _) = parse(&data).unwrap();
        assert_eq!(recs.len(), 2);
        let (ty, mut r) = recs.into_iter().nth(1).unwrap();
        assert_eq!(ty, DAY);
        assert_eq!(r.str().unwrap(), "2026-10-12");
        assert_eq!(r.u().unwrap(), 1_791_763_200_000);
        assert_eq!(r.i().unwrap(), -5);
        assert_eq!(r.opt().unwrap(), None);
        assert_eq!(r.opt().unwrap(), Some(7));
        let _ = std::fs::remove_dir_all(dir);
    }
}
