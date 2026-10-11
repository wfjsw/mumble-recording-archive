//! Export a time range: segments are decoded with ffmpeg, placed on a
//! sample-accurate timeline and mixed into one or more output channels,
//! then streamed out as WAV / FLAC / Opus / MP3, or as a zip of per-track stems.

use crate::config::Config;
use crate::store::{Filter, Segment, Store};
use crate::transcode::{ffmpeg_cmd, sanitize};
use crate::zipstream::ZipStream;
use anyhow::{bail, Context, Result};
use bytes::Bytes;
use futures::StreamExt;
use serde::Serialize;
use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
    Mp3,
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
            "mp3" => Self::Mp3,
            _ => bail!("unknown format {s}"),
        })
    }
    fn ext(self) -> &'static str {
        match self {
            Self::Wav => "wav",
            Self::Flac => "flac",
            Self::Opus => "opus",
            Self::Mp3 => "mp3",
        }
    }
    fn mime(self) -> &'static str {
        match self {
            Self::Wav => "audio/wav",
            Self::Flac => "audio/flac",
            Self::Opus => "audio/ogg",
            Self::Mp3 => "audio/mpeg",
        }
    }
    fn max_channels(self) -> usize {
        match self {
            Self::Wav => 256,
            Self::Flac => 8,
            Self::Opus => 255,
            Self::Mp3 => 2,
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
    let segs = store.segments(req.from, req.to, &req.filter, false)?;
    let mut track_keys: Vec<(i64, i64)> = segs.iter().map(|s| (s.channel_id, s.speaker_id)).collect();
    track_keys.sort();
    track_keys.dedup();

    let speaker_names: BTreeMap<i64, String> = store.speakers()?.into_iter().map(|s| (s.id, s.name)).collect();
    let mut chan_names = BTreeMap::new();
    for (c, _) in &track_keys {
        if !chan_names.contains_key(c) {
            chan_names.insert(*c, store.channel_path(*c)?);
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
    if req.layout != Layout::Stems && channels.len() > req.format.max_channels() {
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
    let ext = if req.layout == Layout::Stems { "zip" } else { req.format.ext() };
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
        items,
    })
}

pub struct Output {
    pub content_type: &'static str,
    pub filename: String,
    pub length: Option<u64>,
    pub body: mpsc::Receiver<Result<Bytes, std::io::Error>>,
}

/// Decode one segment to mono f32 @ 48 kHz.
pub async fn decode(cfg: &Config, seg: &Segment) -> Result<Vec<f32>> {
    let raw = decode_to(cfg, seg, &["-f", "f32le", "-ac", "1", "-ar", "48000"]).await?;
    Ok(raw.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect())
}

/// Run ffmpeg over a segment's stored bytes (fed through stdin) with the given output args.
pub async fn decode_to(cfg: &Config, seg: &Segment, out_args: &[&str]) -> Result<Vec<u8>> {
    let dir = cfg.public_dir();
    let s = seg.clone();
    let data = tokio::task::spawn_blocking(move || Store::read_audio(&dir, &s)).await??;
    let mut cmd = ffmpeg_cmd(cfg);
    cmd.args(["-f", "ogg", "-i", "pipe:0"]).args(out_args).arg("pipe:1");
    cmd.stdout(Stdio::piped()).stderr(Stdio::null()).stdin(Stdio::piped());
    let mut child = cmd.spawn().context("spawn ffmpeg")?;
    let mut stdin = child.stdin.take().unwrap();
    let feeder = tokio::spawn(async move {
        let _ = stdin.write_all(&data).await;
    });
    let mut raw = vec![];
    child.stdout.take().unwrap().read_to_end(&mut raw).await?;
    let _ = feeder.await;
    let status = child.wait().await?;
    if !status.success() && raw.is_empty() {
        bail!("ffmpeg could not decode segment {}", seg.id);
    }
    Ok(raw)
}

/// Render interleaved f32 blocks for `nch` output channels.
fn render(cfg: Arc<Config>, items: Vec<(Segment, usize, f32)>, nch: usize, from: i64, to: i64) -> mpsc::Receiver<Vec<f32>> {
    let (tx, rx) = mpsc::channel(4);
    tokio::spawn(async move {
        let total = ((to - from) * RATE / 1000) as usize;
        let cfg2 = cfg.clone();
        let mut decoded = futures::stream::iter(items.into_iter().map(move |(s, ch, g)| {
            let cfg = cfg2.clone();
            async move {
                let pcm = match decode(&cfg, &s).await {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!("export: decode segment {} failed: {e:#}", s.id);
                        vec![]
                    }
                };
                let start = (s.start_ms - from) * RATE / 1000;
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

fn encoder_args(format: Format, nch: usize) -> Vec<String> {
    let v: Vec<&str> = match format {
        Format::Flac => vec!["-c:a", "flac", "-compression_level", "5", "-f", "flac"],
        Format::Mp3 => vec!["-c:a", "libmp3lame", "-q:a", "4", "-f", "mp3"],
        Format::Opus => vec!["-c:a", "libopus", "-vbr", "on", "-application", "voip", "-f", "ogg"],
        Format::Wav => vec![],
    };
    let mut v: Vec<String> = v.into_iter().map(String::from).collect();
    if format == Format::Opus {
        v.splice(0..0, ["-b:a".to_string(), format!("{}k", 32 * nch.max(1))]);
        if nch > 2 {
            v.splice(0..0, ["-mapping_family".to_string(), "255".to_string()]);
        }
    }
    v
}

/// Encode f32 blocks with ffmpeg into a temp file (so containers get proper
/// duration/seek headers), then hand the file to `sink` chunk by chunk.
async fn encode_stream(
    cfg: &Config,
    mut blocks: mpsc::Receiver<Vec<f32>>,
    nch: usize,
    format: Format,
    meta: &[(String, String)],
    mut sink: impl FnMut(Bytes) -> futures::future::BoxFuture<'static, bool>,
) -> Result<()> {
    let dir = cfg.recording.data_dir.join("tmp");
    tokio::fs::create_dir_all(&dir).await?;
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tmp = TempFile(dir.join(format!(
        "export-{}-{}.{}",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        format.ext()
    )));
    let layout = match nch {
        1 => "mono".to_string(),
        2 => "stereo".to_string(),
        n => format!("{n}c"), // unordered: tracks are not speaker positions
    };
    let mut cmd = ffmpeg_cmd(cfg);
    cmd.args(["-y", "-f", "f32le", "-ar", "48000", "-ch_layout", &layout, "-i", "pipe:0"]);
    for (k, v) in meta {
        cmd.arg("-metadata").arg(format!("{k}={v}"));
    }
    cmd.args(encoder_args(format, nch)).arg(&tmp.0);
    cmd.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped());
    let mut child = cmd.spawn().context("spawn ffmpeg")?;
    let mut stdin = child.stdin.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let err_task = tokio::spawn(async move {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s).await;
        s
    });
    while let Some(b) = blocks.recv().await {
        let bytes: Vec<u8> = b.iter().flat_map(|v| v.to_le_bytes()).collect();
        stdin.write_all(&bytes).await?;
    }
    drop(stdin);
    let status = child.wait().await?;
    if !status.success() {
        bail!("ffmpeg encoder failed: {}", err_task.await.unwrap_or_default().trim());
    }
    let mut f = tokio::fs::File::open(&tmp.0).await?;
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = f.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        if !sink(Bytes::copy_from_slice(&buf[..n])).await {
            bail!("client went away");
        }
    }
    Ok(())
}

struct TempFile(std::path::PathBuf);
impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
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
                        "label": c.label, "channelId": c.tracks[0].0, "speakerId": c.tracks[0].1,
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
                        let meta = vec![("title".to_string(), c.label.clone()), ("date".to_string(), date.clone())];
                        let (ctx, mut crx) = mpsc::channel::<Bytes>(8);
                        let cfg = cfg.clone();
                        let enc = tokio::spawn(async move {
                            encode_stream(&cfg, blocks, 1, format, &meta, move |b| {
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
        let labels = plan.channels.iter().map(|c| format!("{}={}", c.index + 1, c.label)).collect::<Vec<_>>().join("; ");
        let meta = vec![
            ("title".to_string(), plan.filename.clone()),
            ("comment".to_string(), format!("channels: {labels}")),
            ("date".to_string(), date),
        ];
        let tx2 = tx.clone();
        let res = encode_stream(&cfg, blocks, nch, format, &meta, move |b| {
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
