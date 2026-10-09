//! Comtech Drop & Insert++ (D&I++): n × 64 kbit/s timeslots taken from an
//! E1 and carried in frames of 2944 bits — 64 overhead bits and 2880 data
//! bits, so the satellite rate is 46/45 of the data rate (CDM-625 manual
//! §11; CDM-600 likewise). The frame layout is not published; it was found
//! on a CDM-600L carrier (one timeslot, V.35-scrambled — a timeslot that
//! turned out to hold sub-rate channels, not G.711; see `e1::Coding`):
//!
//! | bits      | what                                   |
//! |-----------|----------------------------------------|
//! | 0–23      | header `000001010111101000111000`      |
//! | 24–599    | data: 72 bytes                         |
//! | 600–609   | overhead `0111111111`                  |
//! | 610–1185  | data                                   |
//! | 1186–1195 | overhead `0111111111`                  |
//! | 1196–1771 | data                                   |
//! | 1772–1781 | overhead `0111111111`                  |
//! | 1782–2357 | data                                   |
//! | 2358–2367 | overhead, one bit varying (status)     |
//! | 2368–2943 | data                                   |
//!
//! The data are the timeslots' bytes, MSB first, in order: 360 a frame
//! (45 ms of one 64 kbit/s channel). With several timeslots the bytes are
//! taken to alternate between them, which is not yet confirmed on a carrier.

/// Bits per frame.
pub const FRAME: usize = 2944;
/// Data bytes per frame.
pub const DATA_BYTES: usize = 360;
/// The frame header, bit 0 first.
const HEADER: [u8; 24] = [
    0, 0, 0, 0, 0, 1, 0, 1, 0, 1, 1, 1, 1, 0, 1, 0, 0, 0, 1, 1, 1, 0, 0, 0,
];
/// The overhead between data blocks 1–4, bit 0 first.
const MIDDLE: [u8; 10] = [0, 1, 1, 1, 1, 1, 1, 1, 1, 1];
/// Where the data blocks start; each is 576 bits.
const BLOCKS: [usize; 5] = [24, 610, 1196, 1782, 2368];
const BLOCK_BITS: usize = 576;
/// Header bit errors allowed.
const HEADER_ERRORS: usize = 2;
/// Frames found in a row to declare alignment; misses in a row to lose it.
const ALIGN: usize = 3;
const LOSE: u32 = 3;

fn header_errors(bits: &[u8], at: usize) -> usize {
    HEADER
        .iter()
        .zip(&bits[at..at + 24])
        .filter(|(a, b)| a != b)
        .count()
}

/// Whether a frame starting at `at` looks right: the header and the three
/// fixed middle overheads (a strong check — 54 known bits).
fn frame_ok(bits: &[u8], at: usize) -> bool {
    header_errors(bits, at) <= HEADER_ERRORS
        && [600, 1186, 1772]
            .iter()
            .map(|&o| {
                MIDDLE
                    .iter()
                    .zip(&bits[at + o..at + o + 10])
                    .filter(|(a, b)| a != b)
                    .count()
            })
            .sum::<usize>()
            <= 3
}

/// What the deframer has found.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DiPlusStats {
    pub locked: bool,
    pub frames: u64,
    /// Frames whose header or overheads were wrong while aligned, and
    /// alignments lost.
    pub bad_frames: u64,
    pub losses: u64,
}

/// D&I++ deframing: bits in, the timeslots' bytes out.
#[derive(Default)]
pub struct DiPlusRx {
    bits: Vec<u8>,
    /// Aligned: where the next frame starts in `bits`.
    at: Option<usize>,
    misses: u32,
    pub stats: DiPlusStats,
}

impl DiPlusRx {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bits: &[u8], out: &mut Vec<u8>) {
        self.bits.extend_from_slice(bits);
        if self.at.is_none() && !self.align() {
            return;
        }
        let mut used = 0;
        while let Some(at) = self.at {
            if at + FRAME > self.bits.len() {
                used = at;
                break;
            }
            if frame_ok(&self.bits, at) {
                self.misses = 0;
            } else {
                self.misses += 1;
                self.stats.bad_frames += 1;
                if self.misses >= LOSE {
                    self.at = None;
                    self.stats.locked = false;
                    self.stats.losses += 1;
                    used = at;
                    break;
                }
            }
            for &b in &BLOCKS {
                let block = &self.bits[at + b..at + b + BLOCK_BITS];
                out.extend(
                    block
                        .as_chunks::<8>()
                        .0
                        .iter()
                        .map(|c| c.iter().fold(0u8, |a, &v| (a << 1) | v)),
                );
            }
            self.stats.frames += 1;
            self.at = Some(at + FRAME);
            used = at + FRAME;
        }
        self.bits.drain(..used);
        if let Some(at) = self.at {
            self.at = Some(at - used);
        } else if !self.bits.is_empty() {
            self.align();
        }
    }

    /// Look for frames `ALIGN` in a row at any offset.
    fn align(&mut self) -> bool {
        let need = ALIGN * FRAME + FRAME;
        if self.bits.len() < need {
            return false;
        }
        let found = (0..FRAME).find(|&p| (0..ALIGN).all(|k| frame_ok(&self.bits, p + k * FRAME)));
        match found {
            Some(p) => {
                self.bits.drain(..p);
                self.at = Some(0);
                self.misses = 0;
                self.stats.locked = true;
                true
            }
            None => {
                let keep = self.bits.len() - FRAME;
                self.bits.drain(..keep);
                false
            }
        }
    }
}

/// D&I++ framing (for tests and test signals): 360 data bytes in, 2944 bits
/// out, the varying overhead bit held at 0.
pub fn frame(data: &[u8; DATA_BYTES], out: &mut Vec<u8>) {
    let mut f = vec![0u8; FRAME];
    f[..24].copy_from_slice(&HEADER);
    for o in [600, 1186, 1772] {
        f[o..o + 10].copy_from_slice(&MIDDLE);
    }
    // The last overhead as seen on the CDM-600L, its varying bit at 0.
    f[2358..2368].copy_from_slice(&[0, 0, 1, 1, 0, 1, 0, 0, 1, 1]);
    for (k, &b) in BLOCKS.iter().enumerate() {
        for (i, &byte) in data[k * 72..(k + 1) * 72].iter().enumerate() {
            for j in 0..8 {
                f[b + 8 * i + j] = (byte >> (7 - j)) & 1;
            }
        }
    }
    out.extend_from_slice(&f);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_at_any_offset() {
        let mut bits = vec![1, 1, 0, 1, 0];
        let mut sent = Vec::new();
        for k in 0..12u32 {
            let mut d = [0u8; DATA_BYTES];
            for (i, v) in d.iter_mut().enumerate() {
                *v = (i as u32 * 7 + k * 13) as u8;
            }
            frame(&d, &mut bits);
            sent.extend_from_slice(&d);
        }
        let mut rx = DiPlusRx::new();
        let mut out = Vec::new();
        for c in bits.chunks(1000) {
            rx.push(c, &mut out);
        }
        assert!(rx.stats.locked);
        assert!(out.len() >= 8 * DATA_BYTES, "{}", out.len());
        // Five stray bits, then every frame whole.
        assert_eq!(out[..], sent[..out.len()]);
        assert_eq!(rx.stats.bad_frames, 0);
    }

    #[test]
    fn random_bits_do_not_align() {
        let mut s = 99u64;
        let bits: Vec<u8> = (0..60_000)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                (s & 1) as u8
            })
            .collect();
        let mut rx = DiPlusRx::new();
        let mut out = Vec::new();
        rx.push(&bits, &mut out);
        assert!(!rx.stats.locked && out.is_empty());
    }
}
