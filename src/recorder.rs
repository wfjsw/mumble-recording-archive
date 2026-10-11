//! Segment writer thread. The voice loop must never block on disk or SQLite
//! (a stall overflows the UDP socket and loses voice), so all file and
//! database work for in-progress bursts happens here.
//!
//! In-progress bursts live in `public/raw/` with their metadata in the file
//! name (`{start}-{session}-{speaker}-{channel}-{target}.opus`); the database
//! row is only written when the burst is complete. After a crash, raw files
//! without a row are recovered from their name and Ogg length.

use crate::config::Config;
use crate::live::Live;
use crate::ogg::OggOpusWriter;
use crate::opus;
use crate::store::Store;
use std::collections::HashMap;
use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug, Clone, Copy)]
pub struct BurstMeta {
    pub speaker: i64,
    pub channel: i64,
    pub session: u32,
    pub target: u8,
    pub start_ms: i64,
}

impl BurstMeta {
    pub fn raw_name(&self) -> String {
        format!("raw/{}-{}-{}-{}-{}.opus", self.start_ms, self.session, self.speaker, self.channel, self.target)
    }

    /// Inverse of [`raw_name`] (file name only).
    pub fn parse_raw_name(name: &str) -> Option<Self> {
        let stem = name.strip_suffix(".opus")?;
        let p: Vec<&str> = stem.split('-').collect();
        if p.len() != 5 {
            return None;
        }
        Some(Self {
            start_ms: p[0].parse().ok()?,
            session: p[1].parse().ok()?,
            speaker: p[2].parse().ok()?,
            channel: p[3].parse().ok()?,
            target: p[4].parse().ok()?,
        })
    }
}

enum Cmd {
    Open { key: u64, meta: BurstMeta },
    Packet { key: u64, data: Vec<u8>, samples: i64 },
    Close { key: u64 },
    Flush,
}

pub struct Recorder {
    tx: std::sync::mpsc::Sender<Cmd>,
    next: AtomicU64,
}

struct Open {
    meta: BurstMeta,
    rel: String,
    path: PathBuf,
    samples: i64,
    writer: Option<OggOpusWriter<BufWriter<File>>>,
}

impl Recorder {
    pub fn spawn(cfg: Arc<Config>, store: Arc<Store>, live: Arc<Live>, transcode: UnboundedSender<i64>) -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<Cmd>();
        std::thread::Builder::new()
            .name("segment-writer".into())
            .spawn(move || {
                let mut open: HashMap<u64, Open> = HashMap::new();
                let public = cfg.public_dir();
                let _ = std::fs::create_dir_all(public.join("raw"));
                for cmd in rx {
                    match cmd {
                        Cmd::Open { key, meta } => {
                            let rel = meta.raw_name();
                            let path = public.join(&rel);
                            let writer = File::create(&path)
                                .map(BufWriter::new)
                                .and_then(|f| OggOpusWriter::new(f, key as u32, "mumble-archive passthrough"));
                            let writer = match writer {
                                Ok(w) => Some(w),
                                Err(e) => {
                                    tracing::error!("cannot create {}: {e}", path.display());
                                    None
                                }
                            };
                            open.insert(key, Open { meta, rel, path, samples: 0, writer });
                        }
                        Cmd::Packet { key, data, samples } => {
                            if let Some(o) = open.get_mut(&key) {
                                if let Some(w) = o.writer.as_mut() {
                                    if let Err(e) = w.write_packet(&data, samples) {
                                        tracing::error!("write {}: {e}", o.path.display());
                                    }
                                }
                                o.samples += samples;
                            }
                        }
                        Cmd::Flush => {
                            for o in open.values_mut() {
                                if let Some(w) = o.writer.as_mut() {
                                    let _ = w.flush();
                                }
                            }
                        }
                        Cmd::Close { key } => {
                            if let Some(o) = open.remove(&key) {
                                close(&cfg, &store, &live, &transcode, o);
                            }
                        }
                    }
                }
                // Channel closed (shutdown): finish everything still open.
                for (_, o) in open.drain() {
                    close(&cfg, &store, &live, &transcode, o);
                }
            })
            .expect("spawn segment writer");
        Self { tx, next: AtomicU64::new(1) }
    }

    /// Start a new burst; returns its key for subsequent calls.
    pub fn open(&self, meta: BurstMeta) -> u64 {
        let key = self.next.fetch_add(1, Ordering::Relaxed);
        let _ = self.tx.send(Cmd::Open { key, meta });
        key
    }

    pub fn packet(&self, key: u64, data: &[u8], samples: i64) {
        let _ = self.tx.send(Cmd::Packet { key, data: data.to_vec(), samples });
    }

    pub fn close(&self, key: u64) {
        let _ = self.tx.send(Cmd::Close { key });
    }

    /// Push buffered audio to disk (crash safety); call about once a second.
    pub fn flush(&self) {
        let _ = self.tx.send(Cmd::Flush);
    }
}

fn close(cfg: &Config, store: &Store, live: &Live, transcode: &UnboundedSender<i64>, o: Open) {
    if let Some(w) = o.writer {
        if let Err(e) = w.finish() {
            tracing::error!("finish {}: {e}", o.path.display());
        }
    }
    let dur_ms = o.samples / opus::SAMPLES_PER_MS;
    if dur_ms < cfg.recording.min_segment_ms {
        let _ = std::fs::remove_file(&o.path);
        return;
    }
    let size = std::fs::metadata(&o.path).map(|m| m.len() as i64).unwrap_or(0);
    let m = o.meta;
    match store.insert_raw_segment(m.speaker, m.channel, m.session, m.target, m.start_ms, m.start_ms + dur_ms, o.samples, &o.rel, size) {
        Ok(id) => {
            let _ = store.touch_speaker(m.speaker, m.start_ms + dur_ms);
            live.touch_speakers();
            live.touch_day(m.start_ms);
            let _ = transcode.send(id);
        }
        Err(e) => tracing::error!("store segment {}: {e:#}", o.rel),
    }
}

/// Register raw bursts left behind by a crash (files without a database row).
pub fn recover_orphans(cfg: &Config, store: &Store, transcode: &UnboundedSender<i64>) -> anyhow::Result<()> {
    let dir = cfg.public_dir().join("raw");
    let Ok(entries) = std::fs::read_dir(&dir) else { return Ok(()) };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let rel = format!("raw/{name}");
        if store.path_in_use(&rel)? {
            continue;
        }
        let Some(m) = BurstMeta::parse_raw_name(&name) else { continue };
        let samples = File::open(e.path()).and_then(crate::ogg::last_granule).unwrap_or(0);
        let dur_ms = samples / opus::SAMPLES_PER_MS;
        if dur_ms < cfg.recording.min_segment_ms {
            let _ = std::fs::remove_file(e.path());
            continue;
        }
        let size = e.metadata().map(|m| m.len() as i64).unwrap_or(0);
        let id = store.insert_raw_segment(m.speaker, m.channel, m.session, m.target, m.start_ms, m.start_ms + dur_ms, samples, &rel, size)?;
        tracing::info!("recovered interrupted burst {rel} ({dur_ms} ms)");
        let _ = transcode.send(id);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_name_roundtrip() {
        let m = BurstMeta { speaker: 12, channel: 3, session: 77, target: 2, start_ms: 1_791_703_693_075 };
        let name = m.raw_name();
        let back = BurstMeta::parse_raw_name(name.strip_prefix("raw/").unwrap()).unwrap();
        assert_eq!((back.speaker, back.channel, back.session, back.target, back.start_ms), (12, 3, 77, 2, 1_791_703_693_075));
        assert!(BurstMeta::parse_raw_name("1791703693075-77.opus").is_none());
    }
}
