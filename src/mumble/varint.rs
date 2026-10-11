//! Mumble's variable-length integer encoding used inside voice packets
//! (not the protobuf varint).

pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn byte(&mut self) -> Option<u64> {
        let b = *self.buf.get(self.pos)?;
        self.pos += 1;
        Some(b as u64)
    }

    pub fn varint(&mut self) -> Option<i64> {
        let v = self.byte()?;
        let r = if v & 0x80 == 0x00 {
            v & 0x7F
        } else if v & 0xC0 == 0x80 {
            (v & 0x3F) << 8 | self.byte()?
        } else if v & 0xF0 == 0xF0 {
            match v & 0xFC {
                0xF0 => self.byte()? << 24 | self.byte()? << 16 | self.byte()? << 8 | self.byte()?,
                0xF4 => {
                    let mut r = 0u64;
                    for _ in 0..8 {
                        r = r << 8 | self.byte()?;
                    }
                    r
                }
                0xF8 => return Some(!self.varint()?),
                _ => return Some(!((v & 0x03) as i64)),
            }
        } else if v & 0xF0 == 0xE0 {
            (v & 0x0F) << 24 | self.byte()? << 16 | self.byte()? << 8 | self.byte()?
        } else {
            // 110xxxxx
            (v & 0x1F) << 16 | self.byte()? << 8 | self.byte()?
        };
        Some(r as i64)
    }

    pub fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let s = self.buf.get(self.pos..end)?;
        self.pos = end;
        Some(s)
    }

    pub fn rest(&self) -> &'a [u8] {
        &self.buf[self.pos.min(self.buf.len())..]
    }
}

pub fn write(out: &mut Vec<u8>, v: u64) {
    if v < 0x80 {
        out.push(v as u8);
    } else if v < 0x4000 {
        out.extend_from_slice(&[(v >> 8) as u8 | 0x80, v as u8]);
    } else if v < 0x20_0000 {
        out.extend_from_slice(&[(v >> 16) as u8 | 0xC0, (v >> 8) as u8, v as u8]);
    } else if v < 0x1000_0000 {
        out.extend_from_slice(&[(v >> 24) as u8 | 0xE0, (v >> 16) as u8, (v >> 8) as u8, v as u8]);
    } else if v < 0x1_0000_0000 {
        out.push(0xF0);
        out.extend_from_slice(&(v as u32).to_be_bytes());
    } else {
        out.push(0xF4);
        out.extend_from_slice(&v.to_be_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        for v in [0u64, 1, 127, 128, 300, 16383, 16384, 2_000_000, 300_000_000, 4_000_000_000, 1 << 40] {
            let mut b = vec![];
            write(&mut b, v);
            assert_eq!(Reader::new(&b).varint(), Some(v as i64), "{v}");
        }
        assert_eq!(Reader::new(&[0xFC]).varint(), Some(-1));
    }
}
