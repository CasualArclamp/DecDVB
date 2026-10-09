//! E1 (2.048 Mbit/s) primary-rate framing, ITU-T G.704 §2.3, and G.711
//! A-law: what an E1 carried transparently over a satellite modem looks like
//! once the FEC is undone — 8000 frames a second of 32 eight-bit timeslots.
//! Timeslot 0 carries the frame alignment signal (FAS) x0011011 in every
//! other frame and, in the frames between, a 1 in its second bit (G.704
//! Table 4a); timeslot 16 carries channel-associated signalling (CAS) when
//! used, its multiframe marked by 0000 in frame 0 of each 16 (Table 9);
//! timeslots 1–15 and 17–31 carry 64 kbit/s channels — voice as G.711 A-law.
//!
//! [`E1Rx`] finds the frame alignment in a bit stream at any offset, holds
//! it as G.706 §4.1 does (lost after three bad FAS in a row), and hands out
//! frames; [`alaw`] turns a channel's bytes into samples.

/// Bits per frame: 32 timeslots of 8.
pub const FRAME_BITS: usize = 256;
pub const TIMESLOTS: usize = 32;
/// Frames per second.
pub const FRAME_RATE: u32 = 8000;
/// The frame alignment signal, bits 2–8 of timeslot 0 (G.704 Table 4a).
const FAS: u8 = 0b001_1011;
/// FAS frames (every other frame) seen in a row to declare alignment.
const ALIGN_FAS: usize = 8;
/// Bad FAS in a row that lose it (G.706 §4.1.1).
const LOSE_FAS: u32 = 3;

/// Bits 2–8 of the byte starting at `at` (bit 1 first, MSB first).
fn fas_bits(bits: &[u8], at: usize) -> u8 {
    bits[at + 1..at + 8].iter().fold(0u8, |a, &b| (a << 1) | b)
}

fn byte_at(bits: &[u8], at: usize) -> u8 {
    bits[at..at + 8].iter().fold(0u8, |a, &b| (a << 1) | b)
}

/// What the E1 framer has found.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct E1Stats {
    pub locked: bool,
    pub frames: u64,
    /// FAS words that were wrong while aligned, and alignments lost.
    pub fas_errors: u64,
    pub losses: u64,
    /// Timeslot 16 carries CAS (its multiframe alignment was seen).
    pub cas: bool,
}

/// Frame alignment and frames.
#[derive(Default)]
pub struct E1Rx {
    bits: Vec<u8>,
    /// Aligned: the bit in `bits` where the next frame starts, and whether
    /// that frame is a FAS frame.
    at: Option<(usize, bool)>,
    misses: u32,
    /// Frames since the last CAS multiframe word, if one was seen.
    mfas: Option<u32>,
    pub stats: E1Stats,
}

impl E1Rx {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bits (0/1) in; whole frames (32 timeslot bytes each) out.
    pub fn push(&mut self, bits: &[u8], out: &mut Vec<[u8; TIMESLOTS]>) {
        self.bits.extend_from_slice(bits);
        if self.at.is_none() && !self.align() {
            return;
        }
        let mut used = 0;
        while let Some((at, fas_frame)) = self.at {
            if at + FRAME_BITS > self.bits.len() {
                used = at;
                break;
            }
            let ts0 = &self.bits[at..];
            let ok = if fas_frame {
                fas_bits(ts0, 0) == FAS
            } else {
                ts0[1] == 1
            };
            if ok {
                self.misses = 0;
            } else {
                self.misses += 1;
                self.stats.fas_errors += 1;
                if self.misses >= LOSE_FAS {
                    self.at = None;
                    self.stats.locked = false;
                    self.stats.losses += 1;
                    used = at;
                    break;
                }
            }
            let mut f = [0u8; TIMESLOTS];
            for (k, v) in f.iter_mut().enumerate() {
                *v = byte_at(&self.bits, at + 8 * k);
            }
            // CAS: 0000 in the first half of timeslot 16 once every 16
            // frames (G.704 §5.1.3.2).
            if f[16] >> 4 == 0 {
                if self.mfas == Some(16) {
                    self.stats.cas = true;
                }
                self.mfas = Some(0);
            }
            if let Some(m) = &mut self.mfas {
                *m += 1;
            }
            out.push(f);
            self.stats.frames += 1;
            self.at = Some((at + FRAME_BITS, !fas_frame));
            used = at + FRAME_BITS;
        }
        self.bits.drain(..used);
        if let Some((at, f)) = self.at {
            self.at = Some((at - used, f));
        } else if !self.bits.is_empty() {
            // Lost: look again in what is left.
            self.align();
        }
    }

    /// Look for FAS every 512 bits with the NFAS bit set between, at any
    /// offset; keep the bits from the first FAS frame on.
    fn align(&mut self) -> bool {
        let need = 2 * FRAME_BITS * ALIGN_FAS + 8;
        if self.bits.len() < need + 2 * FRAME_BITS {
            return false;
        }
        let found = (0..2 * FRAME_BITS).find(|&p| {
            (0..ALIGN_FAS).all(|k| {
                let f = p + 2 * FRAME_BITS * k;
                fas_bits(&self.bits, f) == FAS && self.bits[f + FRAME_BITS + 1] == 1
            })
        });
        match found {
            Some(p) => {
                self.bits.drain(..p);
                self.at = Some((0, true));
                self.misses = 0;
                self.mfas = None;
                self.stats.locked = true;
                true
            }
            None => {
                // Keep enough for a frame boundary anywhere.
                let keep = self.bits.len() - 2 * FRAME_BITS;
                self.bits.drain(..keep);
                false
            }
        }
    }
}

/// G.711 A-law to a linear sample, ±1 full scale (G.711 Table 1a: the even
/// bits are inverted on the line, sign 1 = positive).
pub fn alaw(b: u8) -> f32 {
    let a = b ^ 0x55;
    let seg = (a >> 4) & 7;
    let mant = (a & 0x0F) as i32;
    let mag = if seg == 0 {
        (mant << 4) + 8
    } else {
        ((mant << 4) + 0x108) << (seg - 1)
    };
    let v = if a & 0x80 != 0 { mag } else { -mag };
    v as f32 / 32768.0
}

/// A-law from a linear sample (for tests and test signals).
pub fn alaw_encode(x: f32) -> u8 {
    let v = (x.clamp(-1.0, 1.0) * 32767.0) as i32;
    let sign = if v >= 0 { 0x80 } else { 0 };
    let m = v.unsigned_abs().min(32767);
    let (seg, mant) = if m < 256 {
        (0u32, (m >> 4) & 0x0F)
    } else {
        let seg = (31 - m.leading_zeros()) - 7; // 1..=7
        (seg, (m >> (seg + 3)) & 0x0F)
    };
    ((sign | (seg << 4) | mant) as u8) ^ 0x55
}

/// An E1 transmitter's framing (for tests and test signals): 30 or 31
/// channels' bytes per frame in, 256 bits out, FAS/NFAS in timeslot 0.
#[derive(Default)]
pub struct E1Tx {
    n: u64,
}

impl E1Tx {
    pub fn new() -> Self {
        Self::default()
    }

    /// `ts[k]` for timeslots 1..=31 (`ts[0]` is ignored).
    pub fn frame(&mut self, ts: &[u8; TIMESLOTS], out: &mut Vec<u8>) {
        let mut f = *ts;
        f[0] = if self.n.is_multiple_of(2) {
            FAS // Si = 0
        } else {
            0b0100_0000 | 0x1F // NFAS: bit 2 = 1, A = 0, Sa = 1
        };
        self.n += 1;
        for b in f {
            out.extend((0..8).rev().map(|i| (b >> i) & 1));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rng(seed: u64) -> impl FnMut() -> u64 {
        let mut s = seed | 1;
        move || {
            s ^= s >> 12;
            s ^= s << 25;
            s ^= s >> 27;
            s.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
    }

    #[test]
    fn alaw_round_trips_and_matches_g711() {
        // G.711: 0xD5 and 0x55 are the smallest magnitudes, +/-.
        assert!(alaw(0xD5) > 0.0 && alaw(0xD5) < 0.001);
        assert!(alaw(0x55) < 0.0 && alaw(0x55) > -0.001);
        // Full scale: 0xAA (+) and 0x2A (−).
        assert!((alaw(0xAA) - 1.0).abs() < 0.04, "{}", alaw(0xAA));
        assert!((alaw(0x2A) + 1.0).abs() < 0.04);
        for b in 0..=255u8 {
            assert_eq!(alaw_encode(alaw(b)), b, "{b:#04x}");
        }
    }

    #[test]
    fn aligns_at_any_offset_and_hands_out_the_timeslots() {
        let mut tx = E1Tx::new();
        let mut r = rng(4);
        let mut bits = vec![1, 0, 1, 1, 0]; // start mid-frame
        let mut sent = Vec::new();
        for _ in 0..200 {
            let mut ts = [0u8; TIMESLOTS];
            for v in ts.iter_mut().skip(1) {
                *v = r() as u8;
            }
            // Channel 5: a tone, A-law.
            ts[5] = alaw_encode(0.5 * ((sent.len() as f32) * 0.3).sin());
            tx.frame(&ts, &mut bits);
            sent.push(ts);
        }
        let mut rx = E1Rx::new();
        let mut out = Vec::new();
        for c in bits.chunks(1000) {
            rx.push(c, &mut out);
        }
        assert!(rx.stats.locked);
        assert!(out.len() > 180, "{}", out.len());
        // Frames come out whole, timeslots 1..31 as sent.
        let first = sent
            .iter()
            .position(|s| s[1..] == out[0][1..])
            .expect("first frame not found");
        for (k, f) in out.iter().enumerate() {
            assert_eq!(f[1..], sent[first + k][1..], "frame {k}");
        }
        assert_eq!(rx.stats.fas_errors, 0);
    }

    #[test]
    fn random_bits_do_not_align() {
        let mut r = rng(9);
        let bits: Vec<u8> = (0..200_000).map(|_| (r() & 1) as u8).collect();
        let mut rx = E1Rx::new();
        let mut out = Vec::new();
        rx.push(&bits, &mut out);
        assert!(!rx.stats.locked);
        assert!(out.is_empty());
    }
}
