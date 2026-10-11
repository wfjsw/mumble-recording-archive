//! Minimal Opus packet inspection (RFC 6716 §3.1) — enough to know how many
//! samples a packet carries without decoding it.

pub const SAMPLE_RATE: i64 = 48_000;
pub const SAMPLES_PER_MS: i64 = 48;

/// Number of 48 kHz samples encoded in an Opus packet, or None if malformed.
pub fn packet_samples(pkt: &[u8]) -> Option<i64> {
    let toc = *pkt.first()?;
    let config = toc >> 3;
    // frame size in samples @48k (x2 to keep 2.5ms integral)
    let frame: i64 = match config {
        0..=11 => [480, 960, 1920, 2880][(config % 4) as usize],
        12..=15 => [480, 960][(config % 2) as usize],
        _ => [120, 240, 480, 960][(config % 4) as usize],
    };
    let frames = match toc & 0x03 {
        0 => 1,
        1 | 2 => 2,
        _ => (*pkt.get(1)? & 0x3F) as i64,
    };
    let total = frame * frames;
    if total == 0 || total > 5760 {
        return None;
    }
    Some(total)
}

/// CELT-only fullband mono silence frames (20 ms and 10 ms).
const SILENCE_20MS: [u8; 3] = [0xF8, 0xFF, 0xFE];
const SILENCE_10MS: [u8; 3] = [0xF0, 0xFF, 0xFE];

/// Opus packets that decode to `samples` of silence (rounded down to 10 ms).
pub fn silence(samples: i64) -> Vec<&'static [u8]> {
    let mut out = vec![];
    let mut left = samples / 480;
    while left >= 2 {
        out.push(&SILENCE_20MS[..]);
        left -= 2;
    }
    if left == 1 {
        out.push(&SILENCE_10MS[..]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toc() {
        assert_eq!(packet_samples(&[0xF8, 0xFF, 0xFE]), Some(960));
        assert_eq!(packet_samples(&[0xF0, 0xFF, 0xFE]), Some(480));
        // SILK 20ms, code 1 (2 frames)
        assert_eq!(packet_samples(&[(1 << 3) | 1]), Some(1920));
        // CELT 20ms code 3 with 3 frames
        assert_eq!(packet_samples(&[(31 << 3) | 3, 3]), Some(2880));
        assert_eq!(packet_samples(&[]), None);
        assert_eq!(silence(960 * 3 + 480).len(), 4);
    }
}
