//! Compact on-disk record format for day packs.
//!
//! A pack (`audio/YYYY-MM/YYYY-MM-DD.pack`) is a plain concatenation of records:
//!
//! ```text
//! "MAR1"                       magic
//! u32le  body_len              bytes following this field
//! body:  varint seg_id, varint start_ms, varint speaker_id, varint channel_id,
//!        varint samples (48 kHz, exact decoded length), varint pre_skip,
//!        varint packet_count, then packet_count × (varint len, Opus packet)
//! ```
//!
//! Compared with one Ogg stream per segment this saves ~180 bytes of
//! container headers per segment. Records are converted back to Ogg Opus on
//! demand, and packs can be re-indexed by walking the magic/length chain.

use crate::ogg::OggOpusWriter;
use anyhow::{bail, Result};

pub const MAGIC: &[u8; 4] = b"MAR1";

pub struct Record {
    pub seg_id: u64,
    pub start_ms: u64,
    pub speaker_id: u64,
    pub channel_id: u64,
    pub samples: u64,
    pub pre_skip: u64,
    pub packets: Vec<Vec<u8>>,
}

fn put(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

fn take(buf: &[u8], pos: &mut usize) -> Result<u64> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let Some(&b) = buf.get(*pos) else { bail!("truncated record") };
        *pos += 1;
        v |= ((b & 0x7F) as u64) << shift;
        if b & 0x80 == 0 {
            return Ok(v);
        }
    }
    bail!("bad varint")
}

impl Record {
    pub fn encode(&self) -> Vec<u8> {
        let mut body = Vec::with_capacity(32 + self.packets.iter().map(|p| p.len() + 2).sum::<usize>());
        for v in [self.seg_id, self.start_ms, self.speaker_id, self.channel_id, self.samples, self.pre_skip] {
            put(&mut body, v);
        }
        put(&mut body, self.packets.len() as u64);
        for p in &self.packets {
            put(&mut body, p.len() as u64);
            body.extend_from_slice(p);
        }
        let mut out = Vec::with_capacity(8 + body.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(&body);
        out
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        if buf.len() < 8 || &buf[..4] != MAGIC {
            bail!("not a pack record");
        }
        let len = u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize;
        let body = buf.get(8..8 + len).ok_or_else(|| anyhow::anyhow!("truncated record"))?;
        let mut pos = 0;
        let mut f = [0u64; 7];
        for v in f.iter_mut() {
            *v = take(body, &mut pos)?;
        }
        let mut packets = Vec::with_capacity(f[6] as usize);
        for _ in 0..f[6] {
            let n = take(body, &mut pos)? as usize;
            let p = body.get(pos..pos + n).ok_or_else(|| anyhow::anyhow!("truncated packet"))?;
            packets.push(p.to_vec());
            pos += n;
        }
        Ok(Self { seg_id: f[0], start_ms: f[1], speaker_id: f[2], channel_id: f[3], samples: f[4], pre_skip: f[5], packets })
    }

    /// Re-wrap as a standalone Ogg Opus file (sample-accurate end via granule).
    pub fn to_ogg(&self) -> Result<Vec<u8>> {
        let mut w = OggOpusWriter::with_pre_skip(Vec::new(), self.seg_id as u32, "mumble-archive", self.pre_skip as u16)?;
        for p in &self.packets {
            let n = crate::opus::packet_samples(p).unwrap_or(960);
            w.write_packet(p, n)?;
        }
        w.set_end_granule((self.pre_skip + self.samples) as i64);
        Ok(w.finish()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let r = Record {
            seg_id: 77,
            start_ms: 1_791_703_693_075,
            speaker_id: 3,
            channel_id: 2,
            samples: 1500,
            pre_skip: 312,
            packets: vec![vec![0xF8, 0xFF, 0xFE], vec![1; 300]],
        };
        let enc = r.encode();
        let d = Record::decode(&enc).unwrap();
        assert_eq!((d.seg_id, d.start_ms, d.samples, d.pre_skip, d.packets.len()), (77, 1_791_703_693_075, 1500, 312, 2));
        assert_eq!(d.packets[1].len(), 300);
        let ogg = d.to_ogg().unwrap();
        assert_eq!(crate::ogg::last_granule(&ogg[..]).unwrap(), 1812);
    }
}
