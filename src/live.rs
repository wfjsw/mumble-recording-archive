//! Static publishing: everything the web UI needs is written as plain files
//! under the public dir, so the UI can be served by any static web server.
//! Files are regenerated continuously (within ~1 s of a change).
//!
//! ```text
//! public/catalog.json                 days, stats (small; rewritten when recordings change)
//! public/channels.json                channel tree (rewritten when channels change)
//! public/speakers.json                all speakers (rewritten when speakers change)
//! public/index/YYYY-MM-DD.json        per-day segment index, presence, uptime
//! public/audio/YYYY-MM/YYYY-MM-DD.pack   audio records (read with HTTP Range)
//! public/raw/…                        in-progress / fallback Ogg Opus files
//! ```

use crate::config::Config;
use crate::store::Store;
use anyhow::Result;
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Default)]
pub struct Live {
    catalog_dirty: AtomicBool,
    channels_dirty: AtomicBool,
    speakers_dirty: AtomicBool,
    days: Mutex<BTreeSet<String>>,
}

pub fn day_key(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).unwrap_or_default().with_timezone(&chrono::Local).format("%Y-%m-%d").to_string()
}

/// [start, end) of a local calendar day in unix ms.
fn day_bounds(day: &str) -> Option<(i64, i64)> {
    use chrono::TimeZone;
    let d = chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d").ok()?;
    let at = |d: chrono::NaiveDate| {
        chrono::Local.from_local_datetime(&d.and_hms_opt(0, 0, 0).unwrap()).earliest().map(|t| t.timestamp_millis())
    };
    Some((at(d)?, at(d.succ_opt()?)?))
}

impl Live {
    pub fn new() -> Self {
        Self::default()
    }

    /// The day containing `ms` needs its index regenerated.
    pub fn touch_day(&self, ms: i64) {
        self.days.lock().unwrap_or_else(|e| e.into_inner()).insert(day_key(ms));
        self.catalog_dirty.store(true, Ordering::Relaxed);
    }

    pub fn touch_channels(&self) {
        self.channels_dirty.store(true, Ordering::Relaxed);
    }

    pub fn touch_speakers(&self) {
        self.speakers_dirty.store(true, Ordering::Relaxed);
    }
}

fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)
}

pub struct Publisher {
    pub cfg: Arc<Config>,
    pub store: Arc<Store>,
    pub live: Arc<Live>,
}

impl Publisher {
    /// Regenerate everything (CLI `republish`), or just missing day files at startup.
    pub fn rebuild(&self, all: bool) -> Result<()> {
        let dir = self.cfg.public_dir().join("index");
        for d in self.store.days(local_offset_min())? {
            if all || !dir.join(format!("{}.json", d.day)).exists() {
                self.live.days.lock().unwrap().insert(d.day);
            }
        }
        self.live.days.lock().unwrap().insert(day_key(crate::bot::now_ms()));
        self.live.catalog_dirty.store(true, Ordering::Relaxed);
        self.live.channels_dirty.store(true, Ordering::Relaxed);
        self.live.speakers_dirty.store(true, Ordering::Relaxed);
        self.flush()
    }

    pub async fn run(self: Arc<Self>) {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tick.tick().await;
            let me = self.clone();
            match tokio::task::spawn_blocking(move || me.flush()).await {
                Ok(Err(e)) => tracing::error!("publish: {e:#}"),
                Err(e) => tracing::error!("publish task: {e}"),
                _ => {}
            }
        }
    }

    fn flush(&self) -> Result<()> {
        let days: Vec<String> = std::mem::take(&mut *self.live.days.lock().unwrap_or_else(|e| e.into_inner())).into_iter().collect();
        for day in days {
            self.write_day(&day)?;
        }
        if self.live.catalog_dirty.swap(false, Ordering::Relaxed) {
            self.write_catalog()?;
        }
        if self.live.channels_dirty.swap(false, Ordering::Relaxed) {
            // [id, mumbleId, parentId, name, position, removedAt]
            let rows: Vec<serde_json::Value> = self
                .store
                .channels()?
                .into_iter()
                .map(|c| serde_json::json!([c.id, c.mumble_id, c.parent_id, c.name, c.position, c.removed_at]))
                .collect();
            let body = serde_json::json!({ "generatedAt": crate::bot::now_ms(), "channels": rows });
            write_atomic(&self.cfg.public_dir().join("channels.json"), body.to_string().as_bytes())?;
        }
        if self.live.speakers_dirty.swap(false, Ordering::Relaxed) {
            // [id, name, userId, firstSeen, lastSeen]
            let rows: Vec<serde_json::Value> = self
                .store
                .speakers()?
                .into_iter()
                .map(|s| serde_json::json!([s.id, s.name, s.user_id, s.first_seen, s.last_seen]))
                .collect();
            let body = serde_json::json!({ "generatedAt": crate::bot::now_ms(), "speakers": rows });
            write_atomic(&self.cfg.public_dir().join("speakers.json"), body.to_string().as_bytes())?;
        }
        Ok(())
    }

    fn write_day(&self, day: &str) -> Result<()> {
        let Some((from, to)) = day_bounds(day) else { return Ok(()) };
        let segs = self.store.segments_starting(from, to)?;
        let mut paths: Vec<String> = vec![];
        let rows: Vec<serde_json::Value> = segs
            .iter()
            .map(|s| {
                let p = s.path.clone().unwrap_or_default();
                let pi = match paths.iter().position(|x| *x == p) {
                    Some(i) => i,
                    None => {
                        paths.push(p);
                        paths.len() - 1
                    }
                };
                serde_json::json!([
                    s.id, s.speaker_id, s.channel_id, s.start_ms, s.end_ms - s.start_ms, s.target,
                    s.status, pi, s.offset, s.size, s.peaks
                ])
            })
            .collect();
        let presence: Vec<serde_json::Value> = self
            .store
            .presence(from, to)?
            .into_iter()
            .map(|p| serde_json::json!([p.speaker_id, p.channel_id, p.start_ms, p.end_ms]))
            .collect();
        let uptime: Vec<[i64; 2]> = self.store.uptime(from, to)?.into_iter().map(|u| [u.start_ms, u.end_ms]).collect();
        let body = serde_json::json!({
            "day": day, "from": from, "to": to, "generatedAt": crate::bot::now_ms(),
            "columns": ["id", "speakerId", "channelId", "startMs", "durMs", "target", "status", "path", "offset", "size", "peaks"],
            "paths": paths, "segments": rows, "presence": presence, "uptime": uptime,
        });
        write_atomic(&self.cfg.public_dir().join("index").join(format!("{day}.json")), body.to_string().as_bytes())?;
        Ok(())
    }

    fn write_catalog(&self) -> Result<()> {
        let days: Vec<serde_json::Value> = self
            .store
            .days(local_offset_min())?
            .into_iter()
            .filter_map(|d| {
                let (from, to) = day_bounds(&d.day)?;
                Some(serde_json::json!({
                    "day": d.day, "from": from, "to": to, "segments": d.segments, "totalMs": d.total_ms,
                    "file": format!("index/{}.json", d.day),
                }))
            })
            .collect();
        let body = serde_json::json!({
            "version": 1,
            "generatedAt": crate::bot::now_ms(),
            "server": self.cfg.mumble.address,
            "bot": self.cfg.mumble.username,
            "bitrate": self.cfg.recording.bitrate,
            "peaksPerSec": crate::transcode::PEAKS_PER_SEC,
            "stats": self.store.stats()?,
            "days": days,
        });
        write_atomic(&self.cfg.public_dir().join("catalog.json"), body.to_string().as_bytes())?;
        Ok(())
    }
}

fn local_offset_min() -> i64 {
    chrono::Local::now().offset().local_minus_utc() as i64 / 60
}
