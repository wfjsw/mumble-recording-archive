//! Export a time range: segments are decoded in-process (libopus), placed on a
//! sample-accurate timeline and mixed into one or more output channels, then
//! written as WAV / FLAC / Opus, or as a zip of per-track stems.

use crate::codec::{self, ExportEncoder, EXPORT_FRAME};
use crate::config::Config;
use crate::ogg::OggOpusWriter;
use crate::store::{Filter, Segment, Store};
use crate::transcode::sanitize;
use crate::zipstream::ZipStream;
use anyhow::{anyhow, bail, Result};
use bytes::Bytes;
use futures::StreamExt;
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::mpsc;

const RATE: i64 = 48_000;
const BLOCK: usize = 48_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// Everything mixed to one mono channel.
    Downmix,
    /// One output channel per (channel, speaker) track.
    Tracks,
    /// One output channel per Mumble channel (speakers in it mixed).
    Channels,
    /// A zip with one mono file per track, all aligned to the range start.
    Stems,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Wav,
    Flac,
    Opus,
}

impl Layout {
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "downmix" | "" => Self::Downmix,
            "tracks" | "multichannel" => Self::Tracks,
            "channels" => Self::Channels,
            "stems" => Self::Stems,
            _ => bail!("unknown layout {s}"),
        })
    }
}

impl Format {
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "wav" => Self::Wav,
            "flac" | "" => Self::Flac,
            "opus" | "ogg" => Self::Opus,
            _ => bail!("unknown format {s} (wav, flac or opus)"),
        })
    }
    fn ext(self) -> &'static str {
        match self {
            Self::Wav => "wav",
            Self::Flac => "flac",
            Self::Opus => "opus",
        }
    }
    fn mime(self) -> &'static str {
        match self {
            Self::Wav => "audio/wav",
            Self::Flac => "audio/flac",
            Self::Opus => "audio/ogg",
        }
    }
    fn max_channels(self) -> usize {
        match self {
            Self::Wav => 256,
            Self::Flac => 8,
            Self::Opus => 2,
        }
    }
}

pub struct Request {
    pub from: i64,
    pub to: i64,
    pub filter: Filter,
    /// Optional per-track gain, keyed by (channel, speaker).
    pub gains: BTreeMap<(i64, i64), f32>,
    pub layout: Layout,
    pub format: Format,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OutChannel {
    pub index: usize,
    pub label: String,
    pub tracks: Vec<(i64, i64)>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub from: i64,
    pub to: i64,
    pub layout: String,
    pub format: String,
    pub channels: Vec<OutChannel>,
    pub segments: usize,
    pub speech_ms: i64,
    pub filename: String,
    pub warnings: Vec<String>,
    /// One file per output channel (zip) instead of one multichannel file.
    pub split: bool,
    #[serde(skip)]
    items: Vec<(Segment, usize, f32)>,
}

pub fn plan(store: &Store, req: &Request) -> Result<Plan> {
    if req.to <= req.from {
        bail!("empty range");
    }
    if req.to - req.from > 24 * 3_600_000 {
        bail!("range longer than 24h");
    }
    let segs = store.segments(req.from, req.to, &req.filter)?;
    let mut track_keys: Vec<(i64, i64)> = segs.iter().map(|s| (s.channel_id, s.speaker_id)).collect();
    track_keys.sort();
    track_keys.dedup();

    let mut speaker_names = BTreeMap::new();
    let mut chan_names = BTreeMap::new();
    for (c, s) in &track_keys {
        if !chan_names.contains_key(c) {
            chan_names.insert(*c, store.channel_path(*c)?);
        }
        if !speaker_names.contains_key(s) {
            speaker_names.insert(*s, store.speaker_name(*s)?);
        }
    }
    let track_label = |(c, s): (i64, i64)| {
        format!("{} @ {}", speaker_names.get(&s).cloned().unwrap_or_else(|| format!("#{s}")), chan_names[&c])
    };

    let channels: Vec<OutChannel> = match req.layout {
        Layout::Downmix => vec![OutChannel { index: 0, label: "Mix".into(), tracks: track_keys.clone() }],
        Layout::Tracks | Layout::Stems => track_keys
            .iter()
            .enumerate()
            .map(|(i, &k)| OutChannel { index: i, label: track_label(k), tracks: vec![k] })
            .collect(),
        Layout::Channels => {
            let mut by: BTreeMap<i64, Vec<(i64, i64)>> = BTreeMap::new();
            for &k in &track_keys {
                by.entry(k.0).or_default().push(k);
            }
            by.into_iter()
                .enumerate()
                .map(|(i, (c, tracks))| OutChannel { index: i, label: chan_names[&c].clone(), tracks })
                .collect()
        }
    };
    let mut warnings = vec![];
    if channels.is_empty() {
        warnings.push("no recordings in the selected range — output will be silent".into());
    }
    // Multichannel Opus (more than stereo) is poorly supported by players and
    // editors, so multichannel layouts in Opus become one file per output channel.
    let split = req.layout == Layout::Stems || (req.format == Format::Opus && matches!(req.layout, Layout::Tracks | Layout::Channels));
    if !split && channels.len() > req.format.max_channels() {
        bail!("{} supports at most {} channels, selection has {}", req.format.ext(), req.format.max_channels(), channels.len());
    }
    let mut out_of: BTreeMap<(i64, i64), usize> = BTreeMap::new();
    for c in &channels {
        for t in &c.tracks {
            out_of.insert(*t, c.index);
        }
    }
    let items: Vec<(Segment, usize, f32)> = segs
        .into_iter()
        .map(|s| {
            let k = (s.channel_id, s.speaker_id);
            let g = req.gains.get(&k).copied().unwrap_or(1.0);
            (s, out_of[&k], g)
        })
        .collect();
    let speech_ms = items.iter().map(|(s, _, _)| s.end_ms.min(req.to) - s.start_ms.max(req.from)).sum();
    let start = chrono::DateTime::from_timestamp_millis(req.from).unwrap_or_default().with_timezone(&chrono::Local);
    let layout = match req.layout {
        Layout::Downmix => "downmix",
        Layout::Tracks => "multitrack",
        Layout::Channels => "channels",
        Layout::Stems => "stems",
    };
    let ext = if split { "zip" } else { req.format.ext() };
    let filename = format!("mumble_{}_{}s_{layout}.{ext}", start.format("%Y%m%d-%H%M%S"), (req.to - req.from) / 1000);
    Ok(Plan {
        from: req.from,
        to: req.to,
        layout: layout.into(),
        format: req.format.ext().into(),
        channels,
        segments: items.len(),
        speech_ms,
        filename,
        warnings,
        split,
        items,
    })
}

pub struct Output {
    pub content_type: &'static str,
    pub filename: String,
    pub length: Option<u64>,
    pub body: mpsc::Receiver<Result<Bytes, std::io::Error>>,
}

/// Render interleaved f32 blocks for `nch` output channels.
fn render(cfg: Arc<Config>, items: Vec<(Segment, usize, f32)>, nch: usize, from: i64, to: i64) -> mpsc::Receiver<Vec<f32>> {
    let (tx, rx) = mpsc::channel(4);
    tokio::spawn(async move {
        let total = ((to - from) * RATE / 1000) as usize;
        let dir = cfg.public_dir();
        // Decode ahead on blocking threads, in start order.
        let mut decoded = futures::stream::iter(items.into_iter().map(move |(s, ch, g)| {
            let dir = dir.clone();
            async move {
                let start = (s.start_ms - from) * RATE / 1000;
                let id = s.id;
                let pcm = match tokio::task::spawn_blocking(move || codec::decode_segment(&dir, &s)).await {
                    Ok(Ok(p)) => p,
                    Ok(Err(e)) => {
                        tracing::warn!("export: decode segment {id} failed: {e:#}");
                        vec![]
                    }
                    Err(e) => {
                        tracing::warn!("export: decode segment {id} failed: {e}");
                        vec![]
                    }
                };
                (start, ch, g, pcm)
            }
        }))
        .buffered(8);

        let mut active: Vec<(i64, usize, f32, Vec<f32>)> = vec![];
        let mut next = decoded.next().await;
        let mut pos = 0usize;
        while pos < total {
            let end = (pos + BLOCK).min(total);
            while let Some(item) = next.take() {
                if item.0 < end as i64 {
                    active.push(item);
                    next = decoded.next().await;
                } else {
                    next = Some(item);
                    break;
                }
            }
            let mut buf = vec![0f32; (end - pos) * nch];
            for (start, ch, g, pcm) in &active {
                let a = (*start).max(pos as i64);
                let b = (start + pcm.len() as i64).min(end as i64);
                for t in a..b {
                    buf[(t as usize - pos) * nch + ch] += pcm[(t - start) as usize] * g;
                }
            }
            for v in buf.iter_mut() {
                *v = v.clamp(-1.0, 1.0);
            }
            active.retain(|(start, _, _, pcm)| start + pcm.len() as i64 > end as i64);
            if tx.send(buf).await.is_err() {
                return;
            }
            pos = end;
        }
    });
    rx
}

fn wav_header(nch: usize, frames: u64) -> Vec<u8> {
    let data = (frames * nch as u64 * 2).min(u32::MAX as u64 - 100) as u32;
    let mut h = Vec::with_capacity(68);
    h.extend_from_slice(b"RIFF");
    h.extend_from_slice(&(data + 60).to_le_bytes());
    h.extend_from_slice(b"WAVEfmt ");
    h.extend_from_slice(&40u32.to_le_bytes());
    h.extend_from_slice(&0xFFFEu16.to_le_bytes()); // WAVE_FORMAT_EXTENSIBLE
    h.extend_from_slice(&(nch as u16).to_le_bytes());
    h.extend_from_slice(&(RATE as u32).to_le_bytes());
    h.extend_from_slice(&((RATE as u32) * nch as u32 * 2).to_le_bytes());
    h.extend_from_slice(&(nch as u16 * 2).to_le_bytes());
    h.extend_from_slice(&16u16.to_le_bytes());
    h.extend_from_slice(&22u16.to_le_bytes());
    h.extend_from_slice(&16u16.to_le_bytes());
    // channel mask: speaker positions for mono/stereo, unassigned for multitrack
    h.extend_from_slice(&(if nch == 1 { 4u32 } else if nch == 2 { 3 } else { 0 }).to_le_bytes());
    h.extend_from_slice(&[0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71]);
    h.extend_from_slice(b"data");
    h.extend_from_slice(&data.to_le_bytes());
    h
}

fn to_s16(buf: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(buf.len() * 2);
    for v in buf {
        out.extend_from_slice(&((v * 32767.0).round() as i16).to_le_bytes());
    }
    out
}

/// flacenc input fed by the renderer (runs on a blocking thread).
struct BlockSource {
    rx: mpsc::Receiver<Vec<f32>>,
    nch: usize,
    buf: Vec<i32>,
    pos: usize,
}

impl flacenc::source::Source for BlockSource {
    fn channels(&self) -> usize {
        self.nch
    }
    fn bits_per_sample(&self) -> usize {
        16
    }
    fn sample_rate(&self) -> usize {
        RATE as usize
    }
    fn read_samples<F: flacenc::source::Fill>(
        &mut self,
        block_size: usize,
        dest: &mut F,
    ) -> Result<usize, flacenc::error::SourceError> {
        let want = block_size * self.nch;
        while self.buf.len() - self.pos < want {
            let Some(b) = self.rx.blocking_recv() else { break };
            self.buf.drain(..self.pos);
            self.pos = 0;
            self.buf.extend(b.iter().map(|v| (v * 32767.0).round() as i32));
        }
        let n = (self.buf.len() - self.pos).min(want) / self.nch * self.nch;
        dest.fill_interleaved(&self.buf[self.pos..self.pos + n])?;
        self.pos += n;
        Ok(n / self.nch)
    }
}

fn encode_flac(rx: mpsc::Receiver<Vec<f32>>, nch: usize) -> Result<Vec<u8>> {
    use flacenc::component::BitRepr;
    use flacenc::error::Verify;
    let config = flacenc::config::Encoder::default().into_verified().map_err(|e| anyhow!("flac config: {e:?}"))?;
    let src = BlockSource { rx, nch, buf: vec![], pos: 0 };
    let stream =
        flacenc::encode_with_fixed_block_size(&config, src, config.block_size).map_err(|e| anyhow!("flac encode: {e:?}"))?;
    let mut sink = flacenc::bitsink::ByteSink::new();
    stream.write(&mut sink).map_err(|e| anyhow!("flac write: {e:?}"))?;
    Ok(sink.as_slice().to_vec())
}

async fn encode_opus(mut blocks: mpsc::Receiver<Vec<f32>>, nch: usize) -> Result<Vec<u8>> {
    let mut enc = ExportEncoder::new(nch, 32_000 * nch as i32)?;
    // ~1 s per page: per-page overhead would otherwise dominate silent stretches.
    let mut w = OggOpusWriter::with_head(Vec::new(), rand_serial(), "mumble-archive", nch as u8, enc.pre_skip as u16)?.packets_per_page(50);
    let mut frames = 0i64;
    while let Some(b) = blocks.recv().await {
        frames += (b.len() / nch) as i64;
        for p in enc.push(&b)? {
            w.write_packet(&p, EXPORT_FRAME as i64)?;
        }
    }
    for p in enc.finish()? {
        w.write_packet(&p, EXPORT_FRAME as i64)?;
    }
    w.set_end_granule(enc.pre_skip as i64 + frames);
    Ok(w.finish()?)
}

fn rand_serial() -> u32 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(1)
}

/// Encode rendered blocks as FLAC or Opus; the encoded bytes are passed to `sink`.
async fn encode_stream(
    blocks: mpsc::Receiver<Vec<f32>>,
    nch: usize,
    format: Format,
    mut sink: impl FnMut(Bytes) -> futures::future::BoxFuture<'static, bool>,
) -> Result<()> {
    let data = match format {
        Format::Flac => tokio::task::spawn_blocking(move || encode_flac(blocks, nch)).await??,
        Format::Opus => encode_opus(blocks, nch).await?,
        Format::Wav => bail!("WAV is written directly"),
    };
    for chunk in data.chunks(256 * 1024) {
        if !sink(Bytes::copy_from_slice(chunk)).await {
            bail!("output closed");
        }
    }
    Ok(())
}

pub fn run(cfg: Arc<Config>, plan: Plan, format: Format, stems: bool) -> Output {
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(8);
    let nch = plan.channels.len().max(1);
    let frames = ((plan.to - plan.from) * RATE / 1000) as u64;
    let filename = plan.filename.clone();
    let date = chrono::DateTime::from_timestamp_millis(plan.from).unwrap_or_default().with_timezone(&chrono::Local).to_rfc3339();

    if stems {
        tokio::spawn(async move {
            let res: Result<()> = async {
                let mut zip = ZipStream::new(tx.clone());
                let manifest = serde_json::json!({
                    "from": plan.from, "to": plan.to, "start": date, "sampleRate": RATE,
                    "tracks": plan.channels.iter().map(|c| serde_json::json!({
                        "file": format!("{:02}_{}.{}", c.index + 1, sanitize(&c.label), format.ext()),
                        "label": c.label, "tracks": c.tracks.iter().map(|t| serde_json::json!({ "channelId": t.0, "speakerId": t.1 })).collect::<Vec<_>>(),
                    })).collect::<Vec<_>>(),
                });
                zip.add_file("manifest.json", serde_json::to_vec_pretty(&manifest)?.into()).await?;
                for c in &plan.channels {
                    let items: Vec<_> = plan.items.iter().filter(|(_, o, _)| *o == c.index).map(|(s, _, g)| (s.clone(), 0, *g)).collect();
                    let name = format!("{:02}_{}.{}", c.index + 1, sanitize(&c.label), format.ext());
                    zip.begin(&name).await?;
                    let blocks = render(cfg.clone(), items, 1, plan.from, plan.to);
                    if format == Format::Wav {
                        zip.write(wav_header(1, frames).into()).await?;
                        let mut blocks = blocks;
                        while let Some(b) = blocks.recv().await {
                            zip.write(to_s16(&b).into()).await?;
                        }
                    } else {
                        let (ctx, mut crx) = mpsc::channel::<Bytes>(8);
                        let enc = tokio::spawn(async move {
                            encode_stream(blocks, 1, format, move |b| {
                                let ctx = ctx.clone();
                                Box::pin(async move { ctx.send(b).await.is_ok() })
                            })
                            .await
                        });
                        while let Some(b) = crx.recv().await {
                            zip.write(b).await?;
                        }
                        enc.await??;
                    }
                    zip.end().await?;
                }
                zip.finish().await
            }
            .await;
            if let Err(e) = res {
                tracing::warn!("stems export aborted: {e:#}");
                let _ = tx.send(Err(std::io::Error::other(e.to_string()))).await;
            }
        });
        return Output { content_type: "application/zip", filename, length: None, body: rx };
    }

    let items = plan.items.clone();
    let blocks = render(cfg.clone(), items, nch, plan.from, plan.to);
    let length = (format == Format::Wav).then(|| wav_header(nch, frames).len() as u64 + frames * nch as u64 * 2);
    tokio::spawn(async move {
        if format == Format::Wav {
            let mut blocks = blocks;
            if tx.send(Ok(wav_header(nch, frames).into())).await.is_err() {
                return;
            }
            while let Some(b) = blocks.recv().await {
                if tx.send(Ok(to_s16(&b).into())).await.is_err() {
                    return;
                }
            }
            return;
        }
        let tx2 = tx.clone();
        let res = encode_stream(blocks, nch, format, move |b| {
            let tx = tx2.clone();
            Box::pin(async move { tx.send(Ok(b)).await.is_ok() })
        })
        .await;
        if let Err(e) = res {
            tracing::warn!("export aborted: {e:#}");
            let _ = tx.send(Err(std::io::Error::other(e.to_string()))).await;
        }
    });
    Output { content_type: format.mime(), filename, length, body: rx }
}
