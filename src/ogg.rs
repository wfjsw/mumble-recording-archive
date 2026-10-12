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
    /// Packets not yet written, with the granule after each.
    pending: Vec<(Vec<u8>, i64)>,
    /// Packets per page (1 = a page per packet, e.g. for crash-safe live files).
    per_page: usize,
    end_granule: Option<i64>,
}

impl<W: Write> OggOpusWriter<W> {
    pub fn new(w: W, serial: u32, vendor: &str) -> io::Result<Self> {
        Self::with_pre_skip(w, serial, vendor, 0)
    }

    pub fn with_pre_skip(w: W, serial: u32, vendor: &str, pre_skip: u16) -> io::Result<Self> {
        Self::with_head(w, serial, vendor, 1, pre_skip)
    }

    /// Mono/stereo stream (mapping family 0) with the given pre-skip.
    pub fn with_head(w: W, serial: u32, vendor: &str, channels: u8, pre_skip: u16) -> io::Result<Self> {
        let mut head = b"OpusHead".to_vec();
        head.push(1); // version
        head.push(channels);
        head.extend_from_slice(&pre_skip.to_le_bytes()); // 0 for live pass-through packets
        head.extend_from_slice(&48_000u32.to_le_bytes());
        head.extend_from_slice(&0i16.to_le_bytes());
        head.push(0); // mapping family
        let mut tags = b"OpusTags".to_vec();
        tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
        tags.extend_from_slice(vendor.as_bytes());
        tags.extend_from_slice(&0u32.to_le_bytes());
        let mut me = Self { w, serial, seq: 0, granule: 0, pending: vec![], per_page: 1, end_granule: None };
        me.write_page(&[&head], 0, 0x02)?;
        me.write_page(&[&tags], 0, 0x00)?;
        me.w.flush()?;
        Ok(me)
    }

    /// Put up to `n` packets on one page (fewer if they need more than 255
    /// lacing values). Pages carry ~27 bytes of overhead, which is large next to
    /// short or silent (DTX) packets.
    pub fn packets_per_page(mut self, n: usize) -> Self {
        self.per_page = n.max(1);
        self
    }

    fn write_page(&mut self, packets: &[&[u8]], granule: i64, flags: u8) -> io::Result<()> {
        let mut lacing = vec![];
        for p in packets {
            lacing.extend(std::iter::repeat_n(255u8, p.len() / 255));
            lacing.push((p.len() % 255) as u8);
        }
        if lacing.len() > 255 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "packets too large for one page"));
        }
        let body: usize = packets.iter().map(|p| p.len()).sum();
        let mut page = Vec::with_capacity(27 + lacing.len() + body);
        page.extend_from_slice(b"OggS");
        page.push(0);
        page.push(flags);
        page.extend_from_slice(&granule.to_le_bytes());
        page.extend_from_slice(&self.serial.to_le_bytes());
        page.extend_from_slice(&self.seq.to_le_bytes());
        page.extend_from_slice(&[0, 0, 0, 0]);
        page.push(lacing.len() as u8);
        page.extend_from_slice(&lacing);
        for p in packets {
            page.extend_from_slice(p);
        }
        let c = crc(&page);
        page[22..26].copy_from_slice(&c.to_le_bytes());
        self.seq += 1;
        self.w.write_all(&page)
    }

    /// Write the first `n` pending packets as pages (each page ends on a packet).
    fn flush_pending(&mut self, n: usize, last_flags: u8) -> io::Result<()> {
        let mut done = 0;
        while done < n {
            let mut end = done;
            let mut laces = 0;
            while end < n && end - done < self.per_page {
                let l = self.pending[end].0.len() / 255 + 1;
                if laces + l > 255 && end > done {
                    break;
                }
                laces += l;
                end += 1;
            }
            let granule = self.pending[end - 1].1;
            let pkts: Vec<Vec<u8>> = self.pending[done..end].iter().map(|p| p.0.clone()).collect();
            let refs: Vec<&[u8]> = pkts.iter().map(|p| p.as_slice()).collect();
            let flags = if end == n { last_flags } else { 0 };
            self.write_page(&refs, granule, flags)?;
            done = end;
        }
        self.pending.drain(..n);
        Ok(())
    }

    /// Append one Opus packet carrying `samples` 48 kHz samples. Pages are
    /// written once `per_page` packets wait, keeping the last one back so the
    /// final page can carry EOS.
    pub fn write_packet(&mut self, pkt: &[u8], samples: i64) -> io::Result<()> {
        if self.pending.len() > self.per_page {
            let n = self.pending.len() - 1;
            self.flush_pending(n, 0)?;
        }
        self.granule += samples;
        self.pending.push((pkt.to_vec(), self.granule));
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
        if let (Some(end), Some((_, g))) = (self.end_granule, self.pending.last_mut()) {
            *g = end.min(*g);
        }
        if self.pending.is_empty() {
            // EOS needs a page; an empty one is valid.
            self.write_page(&[], self.granule, 0x04)?;
        } else {
            let n = self.pending.len();
            self.flush_pending(n, 0x04)?;
        }
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
    fn grouped_pages_roundtrip() {
        let mut w = OggOpusWriter::new(Vec::new(), 7, "test").unwrap().packets_per_page(50);
        let pkts: Vec<Vec<u8>> = (0..123).map(|i| vec![0xF8; 1 + (i * 37) % 600]).collect();
        for p in &pkts {
            w.write_packet(p, 960).unwrap();
        }
        let buf = w.finish().unwrap();
        assert_eq!(read_opus_packets(&buf), pkts);
        assert_eq!(last_granule(&buf[..]).unwrap(), 123 * 960);
        // far fewer pages than packets
        assert!(buf.windows(4).filter(|w| w == b"OggS").count() < 20);
    }

    #[test]
    fn write_and_measure() {
        let mut w = OggOpusWriter::new(Vec::new(), 7, "test").unwrap();
        for _ in 0..50 {
            w.write_packet(&[0xF8, 0xFF, 0xFE], 960).unwrap();
        }
        let buf = w.finish().unwrap();
        assert_eq!(last_granule(&buf[..]).unwrap(), 48_000);
    }
}
