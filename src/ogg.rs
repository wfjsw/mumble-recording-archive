//! Tiny Ogg Opus muxer (RFC 7845) used to store received Opus packets
//! verbatim, plus a reader to recover the length of interrupted files.

use std::io::{self, Read, Write};

fn crc_table() -> &'static [u32; 256] {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, e) in t.iter_mut().enumerate() {
            let mut r = (i as u32) << 24;
            for _ in 0..8 {
                r = if r & 0x8000_0000 != 0 { (r << 1) ^ 0x04C1_1DB7 } else { r << 1 };
            }
            *e = r;
        }
        t
    })
}

fn crc(data: &[u8]) -> u32 {
    let t = crc_table();
    data.iter().fold(0u32, |c, &b| (c << 8) ^ t[((c >> 24) as u8 ^ b) as usize])
}

pub struct OggOpusWriter<W: Write> {
    w: W,
    serial: u32,
    seq: u32,
    granule: i64,
    pending: Option<(Vec<u8>, i64)>,
    end_granule: Option<i64>,
}

impl<W: Write> OggOpusWriter<W> {
    pub fn new(w: W, serial: u32, vendor: &str) -> io::Result<Self> {
        Self::with_pre_skip(w, serial, vendor, 0)
    }

    pub fn with_pre_skip(w: W, serial: u32, vendor: &str, pre_skip: u16) -> io::Result<Self> {
        let mut head = b"OpusHead".to_vec();
        head.push(1); // version
        head.push(1); // channels
        head.extend_from_slice(&pre_skip.to_le_bytes()); // 0 for live pass-through packets
        head.extend_from_slice(&48_000u32.to_le_bytes());
        head.extend_from_slice(&0i16.to_le_bytes());
        head.push(0); // mapping family
        let mut tags = b"OpusTags".to_vec();
        tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
        tags.extend_from_slice(vendor.as_bytes());
        tags.extend_from_slice(&0u32.to_le_bytes());
        let mut me = Self { w, serial, seq: 0, granule: 0, pending: None, end_granule: None };
        me.write_page(&head, 0, 0x02)?;
        me.write_page(&tags, 0, 0x00)?;
        me.w.flush()?;
        Ok(me)
    }

    fn write_page(&mut self, packet: &[u8], granule: i64, flags: u8) -> io::Result<()> {
        let mut lacing = vec![255u8; packet.len() / 255];
        lacing.push((packet.len() % 255) as u8);
        if lacing.len() > 255 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "packet too large for one page"));
        }
        let mut page = Vec::with_capacity(27 + lacing.len() + packet.len());
        page.extend_from_slice(b"OggS");
        page.push(0);
        page.push(flags);
        page.extend_from_slice(&granule.to_le_bytes());
        page.extend_from_slice(&self.serial.to_le_bytes());
        page.extend_from_slice(&self.seq.to_le_bytes());
        page.extend_from_slice(&[0, 0, 0, 0]);
        page.push(lacing.len() as u8);
        page.extend_from_slice(&lacing);
        page.extend_from_slice(packet);
        let c = crc(&page);
        page[22..26].copy_from_slice(&c.to_le_bytes());
        self.seq += 1;
        self.w.write_all(&page)
    }

    fn flush_pending(&mut self, flags: u8) -> io::Result<()> {
        if let Some((pkt, g)) = self.pending.take() {
            self.write_page(&pkt, g, flags)?;
        }
        Ok(())
    }

    /// Append one Opus packet carrying `samples` 48 kHz samples.
    /// Packets are written one per page, delayed by one so the final page can carry EOS.
    pub fn write_packet(&mut self, pkt: &[u8], samples: i64) -> io::Result<()> {
        self.flush_pending(0)?;
        self.granule += samples;
        self.pending = Some((pkt.to_vec(), self.granule));
        Ok(())
    }

    /// Granule of the final page (pre-skip + exact sample count), trimming encoder padding.
    pub fn set_end_granule(&mut self, g: i64) {
        self.end_granule = Some(g);
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.w.flush()
    }

    pub fn finish(mut self) -> io::Result<W> {
        if let (Some(end), Some((_, g))) = (self.end_granule, self.pending.as_mut()) {
            *g = end.min(*g);
        }
        self.flush_pending(0x04)?;
        self.w.flush()?;
        Ok(self.w)
    }
}

/// Last granule position in an Ogg stream (= total samples when pre-skip is 0).
pub fn last_granule(mut r: impl Read) -> io::Result<i64> {
    let mut data = vec![];
    r.read_to_end(&mut data)?;
    let mut pos = 0;
    let mut last = 0i64;
    while pos + 27 <= data.len() && &data[pos..pos + 4] == b"OggS" {
        let n = data[pos + 26] as usize;
        if pos + 27 + n > data.len() {
            break;
        }
        let body: usize = data[pos + 27..pos + 27 + n].iter().map(|&b| b as usize).sum();
        if pos + 27 + n + body > data.len() {
            break;
        }
        let g = i64::from_le_bytes(data[pos + 6..pos + 14].try_into().unwrap());
        if g > 0 {
            last = g;
        }
        pos += 27 + n + body;
    }
    Ok(last)
}

/// Pre-skip from an OpusHead packet.
pub fn opus_pre_skip(data: &[u8]) -> u16 {
    all_packets(data).first().filter(|h| h.len() >= 12 && h.starts_with(b"OpusHead")).map(|h| u16::from_le_bytes([h[10], h[11]])).unwrap_or(0)
}

/// Extract the audio packets of an Ogg Opus stream (skipping the two header packets).
pub fn read_opus_packets(data: &[u8]) -> Vec<Vec<u8>> {
    all_packets(data).into_iter().skip(2).collect()
}

fn all_packets(data: &[u8]) -> Vec<Vec<u8>> {
    let mut packets = vec![];
    let mut cur = Vec::new();
    let mut pos = 0;
    while pos + 27 <= data.len() && &data[pos..pos + 4] == b"OggS" {
        let n = data[pos + 26] as usize;
        let Some(lacing) = data.get(pos + 27..pos + 27 + n) else { break };
        let body: usize = lacing.iter().map(|&b| b as usize).sum();
        if pos + 27 + n + body > data.len() {
            break; // truncated final page (interrupted recording)
        }
        let mut off = pos + 27 + n;
        for &l in lacing {
            cur.extend_from_slice(&data[off..off + l as usize]);
            off += l as usize;
            if l < 255 {
                packets.push(std::mem::take(&mut cur));
            }
        }
        pos = off;
    }
    packets
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_and_measure() {
        let mut w = OggOpusWriter::new(Vec::new(), 7, "test").unwrap();
        for _ in 0..50 {
            w.write_packet(&[0xF8, 0xFF, 0xFE], 960).unwrap();
        }
        let buf = w.finish().unwrap();
        assert_eq!(last_granule(&buf[..]).unwrap(), 48_000);
        // known vector: CRC of "OggS" page header check is exercised by ffmpeg in integration tests
    }
}
