//! Background processing of finished talk bursts: the pass-through Opus
//! stream is decoded, split into speech regions (silence is not stored),
//! re-encoded to compact VBR speech Opus (never above the source bitrate) and
//! appended to a day pack, plus a coarse peak envelope for waveform display.
//! All codec work is in-process (libopus) on blocking worker threads.

use crate::bot::now_ms;
use crate::codec::{self, SpeechEncoder};
use crate::config::Config;
use crate::live::Live;
use crate::ogg;
use crate::opus;
use crate::pack::Record;
use crate::store::{Part, Segment, Store};
use anyhow::{bail, Result};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::{mpsc, Semaphore};

/// Stored waveform envelope resolution (<1% of the audio size); the UI draws
/// the exact waveform from decoded audio when zoomed in further.
pub const PEAKS_PER_SEC: usize = 10;

pub struct Transcoder {
    pub cfg: Arc<Config>,
    pub store: Arc<Store>,
    pub live: Arc<Live>,
    pub pack_lock: std::sync::Mutex<()>,
}

impl Transcoder {
    /// Recover segments interrupted by a crash, then queue anything not yet processed.
    pub fn recover(&self, tx: &mpsc::UnboundedSender<i64>) -> Result<()> {
        for seg in self.store.segments_by_status("recording")? {
            let path = self.cfg.public_dir().join(seg.path.as_deref().unwrap_or_default());
            let samples = std::fs::File::open(&path).and_then(ogg::last_granule).unwrap_or(0);
            if samples / opus::SAMPLES_PER_MS < self.cfg.recording.min_segment_ms {
                let _ = std::fs::remove_file(&path);
                self.store.delete_segment(seg.id)?;
                continue;
            }
            let size = std::fs::metadata(&path).map(|m| m.len() as i64).unwrap_or(0);
            self.store.finish_segment(seg.id, seg.start_ms + samples / opus::SAMPLES_PER_MS, samples, size)?;
            tracing::info!("recovered interrupted segment {} ({} ms)", seg.id, samples / 48);
        }
        for seg in self.store.segments_by_status("raw")? {
            let _ = tx.send(seg.id);
        }
        Ok(())
    }

    pub async fn run(self: Arc<Self>, mut rx: mpsc::UnboundedReceiver<i64>) {
        let n = match self.cfg.recording.transcode_workers {
            0 => std::thread::available_parallelism().map(|n| (n.get() / 2).max(1)).unwrap_or(2),
            n => n,
        };
        let sem = Arc::new(Semaphore::new(n));
        while let Some(id) = rx.recv().await {
            let permit = sem.clone().acquire_owned().await.unwrap();
            let me = self.clone();
            tokio::task::spawn_blocking(move || {
                if let Err(e) = me.process(id) {
                    tracing::error!("transcode segment {id}: {e:#}");
                }
                drop(permit);
            });
        }
    }

    fn process(&self, id: i64) -> Result<()> {
        let Some(seg) = self.store.segment(id)? else { return Ok(()) };
        if seg.status != "raw" {
            return Ok(());
        }
        let raw_rel = seg.path.clone().unwrap_or_default();
        let raw = self.cfg.public_dir().join(&raw_rel);
        if !raw.exists() {
            self.store.delete_segment(id)?;
            return Ok(());
        }
        let ids = match self.store_regions(&seg, &raw) {
            Ok(ids) => {
                if !self.cfg.recording.keep_raw {
                    let _ = std::fs::remove_file(&raw);
                }
                ids
            }
            Err(e) => {
                // Keep the pass-through file playable; it's still valid Ogg Opus.
                let size = std::fs::metadata(&raw).map(|m| m.len() as i64).unwrap_or(0);
                self.store.segment_done(id, &raw_rel, None, size, &[], "error", Some(&format!("{e:#}")))?;
                tracing::warn!("segment {id} kept as pass-through: {e:#}");
                vec![id]
            }
        };
        let _ = ids;
        self.live.touch_day(seg.start_ms);
        Ok(())
    }

    /// Decode the burst, find its speech regions and store each region as a pack
    /// record: re-encoded, or the original packets if the source is already small.
    fn store_regions(&self, seg: &Segment, raw: &Path) -> Result<Vec<i64>> {
        let rc = &self.cfg.recording;
        let raw_bytes = std::fs::read(raw)?;
        let src_packets = ogg::read_opus_packets(&raw_bytes);
        let pcm = codec::decode_packets(&src_packets, 0, None)?;
        if pcm.is_empty() || src_packets.is_empty() {
            bail!("no audio decoded");
        }
        let total = pcm.len();

        // Never encode above the source: if the client already sent a low bitrate,
        // keep its packets untouched (re-encoding would only lose quality).
        let src_bytes: usize = src_packets.iter().map(|p| p.len()).sum();
        let src_bps = src_bytes as i64 * 8 * opus::SAMPLE_RATE / total as i64;
        let passthrough = src_bps <= self.cfg.target_bps();

        let mut regions = if rc.trim_silence {
            speech_regions(&pcm, rc.silence_threshold_db, rc.min_silence_ms, rc.silence_pad_ms)
        } else {
            vec![]
        };
        if regions.is_empty() {
            // Nothing above the threshold (or trimming disabled): keep the whole burst.
            regions.push((0, total));
        }

        // Sample offsets of source packet boundaries, for pass-through cutting.
        let mut bounds = vec![0usize];
        for p in &src_packets {
            bounds.push(bounds.last().unwrap() + opus::packet_samples(p).unwrap_or(960) as usize);
        }

        struct Region {
            a: usize,
            b: usize,
            pre_skip: u64,
            packets: Vec<Vec<u8>>,
        }
        let mut out: Vec<Region> = vec![];
        let mut encoder: Option<Result<SpeechEncoder>> = None;
        for (a, b) in regions {
            let r = if passthrough {
                // Widen the region to whole source packets.
                let i = bounds.partition_point(|&x| x <= a).saturating_sub(1);
                let j = bounds.partition_point(|&x| x < b).min(src_packets.len());
                Region { a: bounds[i], b: bounds[j].min(total), pre_skip: 0, packets: src_packets[i..j].to_vec() }
            } else {
                let encoder = encoder.get_or_insert_with(|| SpeechEncoder::new(self.cfg.target_bps()));
                let (pre_skip, packets) = encoder.as_mut().map_err(|e| anyhow::anyhow!("{e:#}"))?.encode(&pcm[a..b])?;
                Region { a, b, pre_skip: pre_skip as u64, packets }
            };
            if !r.packets.is_empty() && r.b > r.a {
                out.push(r);
            }
        }
        if out.is_empty() {
            bail!("no regions produced");
        }

        let start_of = |a: usize| seg.start_ms + a as i64 / opus::SAMPLES_PER_MS;
        let blobs: Vec<Vec<u8>> = out
            .iter()
            .enumerate()
            .map(|(k, r)| {
                Record {
                    seg_id: seg.id as u64 * 1000 + k as u64,
                    start_ms: start_of(r.a) as u64,
                    speaker_id: seg.speaker_id as u64,
                    channel_id: seg.channel_id as u64,
                    samples: (r.b - r.a) as u64,
                    pre_skip: r.pre_skip,
                    packets: r.packets.clone(),
                }
                .encode()
            })
            .collect();
        let local = chrono::DateTime::from_timestamp_millis(seg.start_ms).unwrap_or_default().with_timezone(&chrono::Local);
        let rel = format!("audio/{}/{}.pack", local.format("%Y-%m"), local.format("%Y-%m-%d"));
        let offsets = self.append_pack(&rel, &blobs)?;
        let parts: Vec<Part> = out
            .iter()
            .zip(blobs.iter().zip(offsets))
            .map(|(r, (blob, offset))| Part {
                start_ms: start_of(r.a),
                end_ms: start_of(r.a) + ((r.b - r.a) as i64 / opus::SAMPLES_PER_MS).max(1),
                samples: (r.b - r.a) as i64,
                path: rel.clone(),
                offset,
                size: blob.len() as i64,
                peaks: peaks(&pcm[r.a..r.b]),
            })
            .collect();
        tracing::debug!(
            "segment {}: {} region(s), {:.1}s of {:.1}s kept, {} -> {} bytes{}",
            seg.id,
            parts.len(),
            out.iter().map(|r| r.b - r.a).sum::<usize>() as f64 / 48_000.0,
            total as f64 / 48_000.0,
            raw_bytes.len(),
            blobs.iter().map(|b| b.len()).sum::<usize>(),
            if passthrough { " (source at/below target bitrate: packets copied)" } else { "" }
        );
        self.store.split_segment(seg.id, &parts)
    }

    /// Append records to a day pack. Packing avoids per-file filesystem slack,
    /// which is large relative to a few-KB voice snippet.
    fn append_pack(&self, rel: &str, blobs: &[Vec<u8>]) -> Result<Vec<i64>> {
        use std::io::{Seek, SeekFrom, Write};
        let _g = self.pack_lock.lock().unwrap_or_else(|e| e.into_inner());
        let path = self.cfg.public_dir().join(rel);
        std::fs::create_dir_all(path.parent().unwrap())?;
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
        let mut off = f.seek(SeekFrom::End(0))? as i64;
        let mut offsets = vec![];
        let mut all = Vec::with_capacity(blobs.iter().map(|b| b.len()).sum());
        for b in blobs {
            offsets.push(off);
            off += b.len() as i64;
            all.extend_from_slice(b);
        }
        f.write_all(&all)?;
        f.sync_data()?;
        Ok(offsets)
    }
}

/// Sample ranges containing speech: 10 ms frames whose peak exceeds the
/// threshold, joined across pauses shorter than `min_silence_ms`, then padded
/// by `pad_ms` on each side so onsets and decays are kept.
pub fn speech_regions(pcm: &[f32], threshold_db: f32, min_silence_ms: i64, pad_ms: i64) -> Vec<(usize, usize)> {
    const FRAME: usize = 480;
    let thr = 10f32.powf(threshold_db / 20.0);
    let loud: Vec<bool> = pcm.chunks(FRAME).map(|c| c.iter().any(|v| v.abs() >= thr)).collect();
    let min_gap = (min_silence_ms.max(0) as usize).div_ceil(10);
    let pad = pad_ms.max(0) as usize * opus::SAMPLES_PER_MS as usize;

    let mut runs: Vec<(usize, usize)> = vec![]; // in frames
    let mut i = 0;
    while i < loud.len() {
        if !loud[i] {
            i += 1;
            continue;
        }
        let s = i;
        while i < loud.len() && loud[i] {
            i += 1;
        }
        match runs.last_mut() {
            Some(last) if s - last.1 < min_gap => last.1 = i,
            _ => runs.push((s, i)),
        }
    }
    let mut out: Vec<(usize, usize)> = vec![];
    for (s, e) in runs {
        let a = (s * FRAME).saturating_sub(pad);
        let b = (e * FRAME + pad).min(pcm.len());
        match out.last_mut() {
            Some(last) if a <= last.1 => last.1 = b,
            _ => out.push((a, b)),
        }
    }
    out
}

/// sqrt-scaled 8-bit peak envelope at PEAKS_PER_SEC.
fn peaks(pcm: &[f32]) -> Vec<u8> {
    pcm.chunks(48_000 / PEAKS_PER_SEC).map(|c| peak_byte(c.iter().fold(0f32, |m, v| m.max(v.abs())))).collect()
}

/// sqrt-scaled 8-bit peak, keeps resolution for quiet speech.
fn peak_byte(v: f32) -> u8 {
    (v.min(1.0).sqrt() * 255.0).round() as u8
}

pub fn sanitize(s: &str) -> String {
    let out: String = s
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '_' })
        .take(48)
        .collect();
    if out.is_empty() { "_".into() } else { out }
}

/// Periodically delete recordings older than the configured retention.
pub async fn retention(cfg: Arc<Config>, store: Arc<Store>, live: Arc<Live>) {
    if cfg.recording.retention_days <= 0 {
        return;
    }
    let mut iv = tokio::time::interval(std::time::Duration::from_secs(3600));
    loop {
        iv.tick().await;
        let cutoff = now_ms() - cfg.recording.retention_days * 86_400_000;
        loop {
            let old = match store.segments_before(cutoff, 500) {
                Ok(v) => v,
                Err(e) => {
                    tracing::error!("retention: {e}");
                    break;
                }
            };
            if old.is_empty() {
                break;
            }
            let mut paths = std::collections::BTreeSet::new();
            for s in &old {
                live.touch_day(s.start_ms);
                let _ = store.delete_segment(s.id);
                paths.extend(s.path.clone());
            }
            // Day packs are removed once no segment references them any more.
            for p in paths {
                if !store.path_in_use(&p).unwrap_or(true) {
                    let _ = std::fs::remove_file(cfg.public_dir().join(&p));
                }
            }
            tracing::info!("retention: deleted {} segments", old.len());
        }
        let _ = store.prune_presence_before(cutoff);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regions() {
        let mut pcm = vec![0f32; 48_000 * 4];
        // speech 0.5–1.0 s, short pause, 1.2–1.5 s, long pause, 3.0–3.2 s
        for (a, b) in [(0.5, 1.0), (1.2, 1.5), (3.0, 3.2)] {
            for v in &mut pcm[(a * 48_000.0) as usize..(b * 48_000.0) as usize] {
                *v = 0.3;
            }
        }
        let r = speech_regions(&pcm, -45.0, 500, 150);
        let ms: Vec<(usize, usize)> = r.iter().map(|(a, b)| (a / 48, b / 48)).collect();
        assert_eq!(ms, vec![(350, 1650), (2850, 3350)]);
    }
}
