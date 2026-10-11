//! Streaming zip writer (stored entries + data descriptors), so archives can be
//! sent while they are produced without knowing sizes up front.

use anyhow::{anyhow, Result};
use bytes::Bytes;
use tokio::sync::mpsc;

struct Entry {
    name: String,
    offset: u64,
    crc: u32,
    size: u64,
}

pub struct ZipStream {
    tx: mpsc::Sender<Result<Bytes, std::io::Error>>,
    offset: u64,
    entries: Vec<Entry>,
    cur: Option<(Entry, crc32fast::Hasher)>,
    dos_time: u16,
    dos_date: u16,
}

impl ZipStream {
    pub fn new(tx: mpsc::Sender<Result<Bytes, std::io::Error>>) -> Self {
        use chrono::{Datelike, Timelike};
        let now = chrono::Local::now();
        let dos_time = ((now.hour() << 11) | (now.minute() << 5) | (now.second() / 2)) as u16;
        let dos_date = (((now.year() - 1980).max(0) as u32) << 9 | now.month() << 5 | now.day()) as u16;
        Self { tx, offset: 0, entries: vec![], cur: None, dos_time, dos_date }
    }

    async fn emit(&mut self, b: Bytes) -> Result<()> {
        self.offset += b.len() as u64;
        self.tx.send(Ok(b)).await.map_err(|_| anyhow!("client went away"))
    }

    pub async fn begin(&mut self, name: &str) -> Result<()> {
        let mut h = Vec::new();
        h.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        h.extend_from_slice(&20u16.to_le_bytes());
        h.extend_from_slice(&(0x0008u16 | 0x0800).to_le_bytes()); // data descriptor + UTF-8 names
        h.extend_from_slice(&0u16.to_le_bytes()); // stored
        h.extend_from_slice(&self.dos_time.to_le_bytes());
        h.extend_from_slice(&self.dos_date.to_le_bytes());
        h.extend_from_slice(&[0; 12]); // crc, sizes in descriptor
        h.extend_from_slice(&(name.len() as u16).to_le_bytes());
        h.extend_from_slice(&0u16.to_le_bytes());
        h.extend_from_slice(name.as_bytes());
        let entry = Entry { name: name.into(), offset: self.offset, crc: 0, size: 0 };
        self.emit(h.into()).await?;
        self.cur = Some((entry, crc32fast::Hasher::new()));
        Ok(())
    }

    pub async fn write(&mut self, b: Bytes) -> Result<()> {
        let (e, h) = self.cur.as_mut().ok_or_else(|| anyhow!("no open entry"))?;
        h.update(&b);
        e.size += b.len() as u64;
        self.emit(b).await
    }

    pub async fn end(&mut self) -> Result<()> {
        let (mut e, h) = self.cur.take().ok_or_else(|| anyhow!("no open entry"))?;
        e.crc = h.finalize();
        let mut d = Vec::new();
        d.extend_from_slice(&0x0807_4b50u32.to_le_bytes());
        d.extend_from_slice(&e.crc.to_le_bytes());
        d.extend_from_slice(&(e.size as u32).to_le_bytes());
        d.extend_from_slice(&(e.size as u32).to_le_bytes());
        self.entries.push(e);
        self.emit(d.into()).await
    }

    pub async fn add_file(&mut self, name: &str, data: Bytes) -> Result<()> {
        self.begin(name).await?;
        self.write(data).await?;
        self.end().await
    }

    pub async fn finish(mut self) -> Result<()> {
        let cd_start = self.offset;
        let mut cd = Vec::new();
        for e in &self.entries {
            cd.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            cd.extend_from_slice(&20u16.to_le_bytes());
            cd.extend_from_slice(&20u16.to_le_bytes());
            cd.extend_from_slice(&(0x0008u16 | 0x0800).to_le_bytes());
            cd.extend_from_slice(&0u16.to_le_bytes());
            cd.extend_from_slice(&self.dos_time.to_le_bytes());
            cd.extend_from_slice(&self.dos_date.to_le_bytes());
            cd.extend_from_slice(&e.crc.to_le_bytes());
            cd.extend_from_slice(&(e.size as u32).to_le_bytes());
            cd.extend_from_slice(&(e.size as u32).to_le_bytes());
            cd.extend_from_slice(&(e.name.len() as u16).to_le_bytes());
            cd.extend_from_slice(&[0; 12]); // extra, comment, disk, int attr, ext attr
            cd.extend_from_slice(&(e.offset as u32).to_le_bytes());
            cd.extend_from_slice(e.name.as_bytes());
        }
        let cd_len = cd.len() as u32;
        cd.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        cd.extend_from_slice(&[0; 4]);
        cd.extend_from_slice(&(self.entries.len() as u16).to_le_bytes());
        cd.extend_from_slice(&(self.entries.len() as u16).to_le_bytes());
        cd.extend_from_slice(&cd_len.to_le_bytes());
        cd.extend_from_slice(&(cd_start as u32).to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        self.emit(cd.into()).await
    }
}
