//! Per-speaker reorder buffer. Voice over UDP can arrive out of order; packets
//! are held briefly and released in sequence order so the passthrough stream
//! is written correctly. In-order traffic (e.g. the TCP tunnel) passes
//! straight through.

use crate::mumble::AudioPacket;
use std::collections::BTreeMap;

/// How long a packet may wait for earlier, missing packets.
pub const HOLD_MS: i64 = 150;
/// A sequence number this far behind the last released one is a counter
/// reset (new stream), not a late packet. Sequence units are 10 ms frames.
const RESET_WINDOW: i64 = 500;
/// Upper bound on buffered packets per speaker.
const MAX_PENDING: usize = 64;

pub struct Item {
    pub pkt: AudioPacket,
    /// Arrival time (ms); used for timing decisions instead of release time.
    pub arrival: i64,
}

#[derive(Default)]
pub struct Reorder {
    pending: BTreeMap<i64, Item>,
    /// Sequence and frame step of the last released packet.
    last: Option<(i64, i64)>,
    pub dropped_late: u64,
}

fn step(pkt: &AudioPacket) -> i64 {
    (crate::opus::packet_samples(&pkt.opus).unwrap_or(480) / 480).max(1)
}

impl Reorder {
    /// Queue a packet. Returns false if it was dropped (late duplicate or already released position).
    pub fn push(&mut self, pkt: AudioPacket, arrival: i64) -> bool {
        if let Some((last, _)) = self.last {
            if pkt.sequence <= last {
                if last - pkt.sequence > RESET_WINDOW {
                    // Sender restarted its counter: flush ordering state.
                    self.last = None;
                } else {
                    self.dropped_late += 1;
                    return false;
                }
            }
        }
        if self.pending.contains_key(&pkt.sequence) {
            self.dropped_late += 1;
            return false;
        }
        self.pending.insert(pkt.sequence, Item { pkt, arrival });
        true
    }

    /// Next packet that may be written now: the in-order successor of the last
    /// released packet, or the oldest one once it has waited `HOLD_MS`.
    pub fn pop_ready(&mut self, now: i64) -> Option<Item> {
        let (&seq, first) = self.pending.iter().next()?;
        let contiguous = match self.last {
            None => true,
            // Clients advance the sequence per 10 ms frame; some per packet. Both are "next".
            Some((last, step)) => seq <= last + step,
        };
        if contiguous || now - first.arrival >= HOLD_MS || self.pending.len() > MAX_PENDING {
            let item = self.pending.remove(&seq)?;
            self.last = Some((seq, step(&item.pkt)));
            return Some(item);
        }
        None
    }

    /// Everything still buffered, in order (e.g. when the speaker leaves).
    pub fn drain(&mut self) -> Vec<Item> {
        let items: Vec<Item> = std::mem::take(&mut self.pending).into_values().collect();
        if let Some(i) = items.last() {
            self.last = Some((i.pkt.sequence, step(&i.pkt)));
        }
        items
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkt(seq: i64) -> AudioPacket {
        // 20 ms CELT frame → step 2
        AudioPacket { codec: 4, target: 0, session: 1, sequence: seq, opus: vec![0xF8, 0xFF, 0xFE], terminator: false }
    }

    fn release(r: &mut Reorder, now: i64) -> Vec<i64> {
        std::iter::from_fn(|| r.pop_ready(now)).map(|i| i.pkt.sequence).collect()
    }

    #[test]
    fn in_order_passes_through() {
        let mut r = Reorder::default();
        for s in [0, 2, 4, 6] {
            assert!(r.push(pkt(s), 0));
            assert_eq!(release(&mut r, 0), vec![s]);
        }
    }

    #[test]
    fn reorders_and_drops_late() {
        let mut r = Reorder::default();
        r.push(pkt(0), 0);
        assert_eq!(release(&mut r, 0), vec![0]);
        r.push(pkt(4), 10); // 2 is missing: hold
        assert!(release(&mut r, 20).is_empty());
        r.push(pkt(2), 30); // arrives late but in time
        assert_eq!(release(&mut r, 30), vec![2, 4]);
        r.push(pkt(8), 40); // 6 lost
        assert!(release(&mut r, 100).is_empty());
        assert_eq!(release(&mut r, 40 + HOLD_MS), vec![8]);
        assert!(!r.push(pkt(6), 300), "too late: already played past it");
        assert!(!r.push(pkt(8), 300), "duplicate");
        assert_eq!(r.dropped_late, 2);
    }

    #[test]
    fn counter_reset_is_not_late() {
        let mut r = Reorder::default();
        r.push(pkt(5000), 0);
        release(&mut r, 0);
        assert!(r.push(pkt(0), 10));
        assert_eq!(release(&mut r, 10), vec![0]);
    }

    #[test]
    fn per_packet_sequence_counters() {
        // Some clients advance the sequence by 1 per 20 ms packet.
        let mut r = Reorder::default();
        for s in 0..5 {
            r.push(pkt(s), 0);
            assert_eq!(release(&mut r, 0), vec![s]);
        }
    }
}
