//! In-process Opus decoding/encoding (libopus via the `opus` crate): no
//! external tools, no per-burst process start-up.

use crate::ogg;
use crate::pack::Record;
use crate::store::Segment;
use anyhow::{Context, Result};
use opus::{Application, Bitrate, Channels, Decoder, Encoder, Signal};
use std::path::Path;

pub const RATE: u32 = 48_000;
/// Largest Opus packet duration (120 ms) in samples.
const MAX_FRAME: usize = 5_760;
/// Frame size used for stored speech: 120 ms has the least per-packet overhead.
pub const STORAGE_FRAME: usize = 5_760;

/// Decode a mono stream of Opus packets to f32 @ 48 kHz, dropping `pre_skip`
/// samples and trimming to `samples` when known. A packet that fails to decode
/// is replaced by silence of its nominal length so later audio stays aligned.
pub fn decode_packets(packets: &[Vec<u8>], pre_skip: usize, samples: Option<usize>) -> Result<Vec<f32>> {
    let mut dec = Decoder::new(RATE, Channels::Mono).context("create Opus decoder")?;
    let mut out = Vec::with_capacity(samples.unwrap_or(0) + pre_skip + MAX_FRAME);
    let mut buf = vec![0f32; MAX_FRAME];
    for p in packets {
        match dec.decode_float(p, &mut buf, false) {
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(_) => {
                let n = crate::opus::packet_samples(p).unwrap_or(960) as usize;
                out.resize(out.len() + n, 0.0);
            }
        }
    }
    let start = pre_skip.min(out.len());
    let end = samples.map_or(out.len(), |s| (start + s).min(out.len()));
    out.truncate(end);
    out.drain(..start);
    Ok(out)
}

/// Decode a standalone Ogg Opus file (pass-through bursts, legacy segments).
pub fn decode_ogg(bytes: &[u8]) -> Result<Vec<f32>> {
    let pre_skip = ogg::opus_pre_skip(bytes) as usize;
    let packets = ogg::read_opus_packets(bytes);
    let granule = ogg::last_granule(bytes).unwrap_or(0).max(0) as usize;
    let samples = (granule > pre_skip).then(|| granule - pre_skip);
    decode_packets(&packets, pre_skip, samples)
}

/// Decode a stored segment (pack record or standalone Ogg file) to mono f32 @ 48 kHz.
pub fn decode_segment(public_dir: &Path, seg: &Segment) -> Result<Vec<f32>> {
    use std::io::{Read, Seek, SeekFrom};
    let path = public_dir.join(seg.path.as_deref().unwrap_or_default());
    match seg.offset {
        None => decode_ogg(&std::fs::read(&path).with_context(|| format!("read {}", path.display()))?),
        Some(off) => {
            let mut f = std::fs::File::open(&path).with_context(|| format!("open {}", path.display()))?;
            f.seek(SeekFrom::Start(off as u64))?;
            let mut buf = vec![0u8; seg.size as usize];
            f.read_exact(&mut buf)?;
            let r = Record::decode(&buf)?;
            decode_packets(&r.packets, r.pre_skip as usize, Some(r.samples as usize))
        }
    }
}

/// Mono VBR speech encoder used for the archive.
pub struct SpeechEncoder {
    enc: Encoder,
    pre_skip: usize,
    out: Vec<u8>,
}

impl SpeechEncoder {
    pub fn new(bitrate_bps: i64) -> Result<Self> {
        let mut enc = Encoder::new(RATE, Channels::Mono, Application::Voip).context("create Opus encoder")?;
        enc.set_bitrate(Bitrate::Bits(bitrate_bps.clamp(6_000, 510_000) as i32))?;
        enc.set_vbr(true)?;
        enc.set_complexity(10)?;
        enc.set_signal(Signal::Voice)?;
        // DTX: near-silent frames inside speech cost 1-2 bytes (~4% smaller on speech).
        enc.set_dtx(true)?;
        let pre_skip = enc.get_lookahead()?.max(0) as usize;
        Ok(Self { enc, pre_skip, out: vec![0u8; 4000] })
    }

    /// Encode one independent region. Returns (pre-skip, packets); decoding
    /// yields `pre_skip` warm-up samples, then exactly `pcm`, then padding.
    pub fn encode(&mut self, pcm: &[f32]) -> Result<(u16, Vec<Vec<u8>>)> {
        self.enc.reset_state()?;
        // Flush the encoder's lookahead with silence, then pad to whole frames.
        let mut input = Vec::with_capacity(pcm.len() + self.pre_skip + STORAGE_FRAME);
        input.extend_from_slice(pcm);
        input.resize(pcm.len() + self.pre_skip, 0.0);
        input.resize(input.len().div_ceil(STORAGE_FRAME) * STORAGE_FRAME, 0.0);
        let mut packets = Vec::with_capacity(input.len() / STORAGE_FRAME);
        for frame in input.chunks_exact(STORAGE_FRAME) {
            let n = self.enc.encode_float(frame, &mut self.out)?;
            packets.push(self.out[..n].to_vec());
        }
        Ok((self.pre_skip as u16, packets))
    }
}

/// Opus encoder for exports (mono or stereo, 20 ms frames).
pub struct ExportEncoder {
    enc: Encoder,
    pub channels: usize,
    pub pre_skip: usize,
    pending: Vec<f32>,
    out: Vec<u8>,
}

pub const EXPORT_FRAME: usize = 960;

impl ExportEncoder {
    pub fn new(channels: usize, bitrate_bps: i32) -> Result<Self> {
        let ch = match channels {
            1 => Channels::Mono,
            2 => Channels::Stereo,
            n => anyhow::bail!("Opus export supports 1 or 2 channels, not {n}"),
        };
        let mut enc = Encoder::new(RATE, ch, Application::Voip)?;
        enc.set_bitrate(Bitrate::Bits(bitrate_bps))?;
        enc.set_vbr(true)?;
        // Exported tracks are mostly silence between clips: DTX makes it nearly free.
        enc.set_dtx(true)?;
        let pre_skip = enc.get_lookahead()?.max(0) as usize;
        Ok(Self { enc, channels, pre_skip, pending: vec![], out: vec![0u8; 4000] })
    }

    /// Feed interleaved samples; returns complete packets (each EXPORT_FRAME samples).
    pub fn push(&mut self, interleaved: &[f32]) -> Result<Vec<Vec<u8>>> {
        self.pending.extend_from_slice(interleaved);
        let step = EXPORT_FRAME * self.channels;
        let mut packets = vec![];
        let whole = self.pending.len() / step * step;
        for frame in self.pending[..whole].chunks_exact(step) {
            let n = self.enc.encode_float(frame, &mut self.out)?;
            packets.push(self.out[..n].to_vec());
        }
        self.pending.drain(..whole);
        Ok(packets)
    }

    /// Flush lookahead + remainder with silence.
    pub fn finish(&mut self) -> Result<Vec<Vec<u8>>> {
        let pad = vec![0f32; (self.pre_skip + EXPORT_FRAME) * self.channels];
        let mut packets = self.push(&pad)?;
        if !self.pending.is_empty() {
            let rest = EXPORT_FRAME * self.channels - self.pending.len();
            packets.extend(self.push(&vec![0f32; rest])?);
        }
        Ok(packets)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(n: usize) -> Vec<f32> {
        (0..n).map(|i| (i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.3).collect()
    }

    #[test]
    fn speech_roundtrip_is_sample_aligned() {
        let pcm = tone(48_000 + 1234);
        let mut enc = SpeechEncoder::new(12_000).unwrap();
        let (pre, packets) = enc.encode(&pcm).unwrap();
        assert!(pre > 0);
        let out = decode_packets(&packets, pre as usize, Some(pcm.len())).unwrap();
        assert_eq!(out.len(), pcm.len());
        // lossy, but aligned: correlation with the input must be high
        let dot: f32 = pcm.iter().zip(&out).map(|(a, b)| a * b).sum();
        let norm: f32 = pcm.iter().map(|a| a * a).sum::<f32>().sqrt() * out.iter().map(|b| b * b).sum::<f32>().sqrt();
        assert!(dot / norm > 0.8, "correlation {}", dot / norm);
    }

    #[test]
    fn dtx_keeps_timeline_aligned() {
        // tone, 2 s of digital silence (DTX kicks in), tone again
        let mut pcm = tone(48_000);
        pcm.extend(std::iter::repeat_n(0.0, 96_000));
        pcm.extend(tone(48_000));
        let mut enc = SpeechEncoder::new(12_000).unwrap();
        let (pre, packets) = enc.encode(&pcm).unwrap();
        assert!(packets.iter().any(|p| p.len() <= 2), "expected DTX packets");
        for p in &packets {
            // every stored packet must describe its own duration (the timeline relies on it)
            assert_eq!(crate::opus::packet_samples(p), Some(STORAGE_FRAME as i64), "packet {:?}", &p[..p.len().min(4)]);
        }
        let out = decode_packets(&packets, pre as usize, Some(pcm.len())).unwrap();
        assert_eq!(out.len(), pcm.len());
        // the tone after the silence must come back at the right place
        let tail = &out[out.len() - 24_000..];
        let energy: f32 = tail.iter().map(|v| v * v).sum::<f32>() / tail.len() as f32;
        assert!(energy > 0.01, "energy {energy}");
        let quiet = &out[60_000..130_000];
        assert!(quiet.iter().all(|v| v.abs() < 0.05));
    }

    #[test]
    fn record_and_ogg_paths_agree() {
        let pcm = tone(30_000);
        let mut enc = SpeechEncoder::new(16_000).unwrap();
        let (pre, packets) = enc.encode(&pcm).unwrap();
        let r = Record {
            seg_id: 1,
            start_ms: 0,
            speaker_id: 1,
            channel_id: 1,
            samples: pcm.len() as u64,
            pre_skip: pre as u64,
            packets: packets.clone(),
        };
        let a = decode_packets(&packets, pre as usize, Some(pcm.len())).unwrap();
        let b = decode_ogg(&r.to_ogg().unwrap()).unwrap();
        assert_eq!(a, b);
    }
}
