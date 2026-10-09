//! What a modem's data bits carry: a synchronous serial stream with no
//! framing of its own, typically HDLC frames (IP over a VSAT link) or an
//! MPEG transport stream, often scrambled for energy dispersal. Neither the
//! format nor the scrambler is signalled, so both are found by trying each
//! candidate on a stretch of data and keeping the one under which frames
//! check out: HDLC frames whose FCS is right, TS packets whose sync bytes
//! recur every 188 bytes, or E1 frames whose alignment signal recurs every
//! 512 bits (an E1 carried transparently, see [`crate::e1`]).
//!
//! The scramblers tried are the RCV-20x's polynomial 1 + x² + x³ + x⁹ + x¹²
//! (as a self-synchronising descrambler, either way round, and additive per
//! frame), and the self-synchronising ones of ITU-T V.35 (taps 3, 20,
//! without its 32-bit run counter) and V.29/V.27 (taps 18, 23).

use crate::e1::{E1Rx, TIMESLOTS};
use crate::tpc2964::additive_sequence;

/// How the data might be scrambled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Descrambler {
    None,
    /// Self-synchronising: out = in ⊕ in[n−t₁] ⊕ in[n−t₂] … for the taps.
    SelfSync(&'static [usize]),
    /// The additive (2,3,9,12)/475h sequence, restarted every frame (see
    /// [`additive_sequence`] for `last` and `reversed`).
    Additive {
        last: bool,
        reversed: bool,
    },
}

const SELF_SYNC: [&[usize]; 4] = [&[2, 3, 9, 12], &[3, 9, 10, 12], &[3, 20], &[18, 23]];

impl Descrambler {
    pub fn all() -> Vec<Descrambler> {
        let mut v = vec![Descrambler::None];
        v.extend(SELF_SYNC.iter().map(|&t| Descrambler::SelfSync(t)));
        for last in [false, true] {
            for reversed in [false, true] {
                v.push(Descrambler::Additive { last, reversed });
            }
        }
        v
    }

    pub fn describe(&self) -> String {
        match self {
            Descrambler::None => "not scrambled".into(),
            Descrambler::SelfSync(t) => format!(
                "self-synchronising descrambler, taps {}",
                t.iter()
                    .map(|t| t.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Descrambler::Additive { last, reversed } => format!(
                "additive (2,3,9,12)/475h per frame, output {}, preset {}",
                if *last { "stage 12" } else { "feedback" },
                if *reversed { "reversed" } else { "as written" }
            ),
        }
    }
}

/// A [`Descrambler`] with its state.
#[derive(Clone)]
struct Running {
    kind: Descrambler,
    /// Self-synchronising: the last received bits, newest in bit 0.
    history: u32,
    /// Additive: the per-frame sequence.
    seq: Vec<u8>,
}

impl Running {
    fn new(kind: Descrambler, frame_len: usize) -> Self {
        let seq = match kind {
            Descrambler::Additive { last, reversed } => {
                additive_sequence(last, reversed, frame_len)
            }
            _ => Vec::new(),
        };
        Running {
            kind,
            history: 0,
            seq,
        }
    }

    /// Descramble one frame's bits in place.
    fn frame(&mut self, bits: &mut [u8]) {
        match self.kind {
            Descrambler::None => {}
            Descrambler::SelfSync(taps) => {
                for b in bits.iter_mut() {
                    let x = *b;
                    let mut y = x;
                    for &t in taps {
                        y ^= ((self.history >> (t - 1)) & 1) as u8;
                    }
                    self.history = (self.history << 1) | x as u32;
                    *b = y;
                }
            }
            Descrambler::Additive { .. } => {
                for (b, s) in bits.iter_mut().zip(&self.seq) {
                    *b ^= s;
                }
            }
        }
    }
}

/// HDLC deframing (ISO/IEC 13239): flags 7Eh, zero-bit stuffing, bytes
/// sent LSB first, a 16- or 32-bit FCS.
#[derive(Clone, Default)]
pub struct Hdlc {
    ones: u32,
    bits: Vec<u8>,
    /// Between flags (not aborted, not overlong).
    open: bool,
    pub good: u64,
    pub bad: u64,
}

/// Which FCS a frame carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fcs {
    Crc16,
    Crc32,
}

/// The longest frame taken, in bits.
const HDLC_MAX: usize = 9000 * 8;

fn crc16_x25(data: &[u8]) -> u16 {
    let mut c = 0xFFFFu16;
    for &b in data {
        c ^= b as u16;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                (c >> 1) ^ 0x8408
            } else {
                c >> 1
            };
        }
    }
    c
}

fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                (c >> 1) ^ 0xEDB8_8320
            } else {
                c >> 1
            };
        }
    }
    c
}

impl Hdlc {
    /// Push bits; each frame whose FCS checks goes to `out` without its FCS.
    pub fn push(&mut self, bits: &[u8], out: &mut Vec<(Vec<u8>, Fcs)>) {
        for &b in bits {
            if b == 1 {
                self.ones += 1;
                if self.ones > 6 {
                    // Abort (or idle ones): drop the frame until a flag.
                    self.open = false;
                    self.bits.clear();
                    continue;
                }
                self.bits.push(1);
            } else {
                match self.ones {
                    6 => {
                        // A flag, 01111110: its first seven bits went in.
                        let n = self.bits.len().saturating_sub(7);
                        self.bits.truncate(n);
                        if self.open {
                            self.close(out);
                        }
                        self.bits.clear();
                        self.open = true;
                    }
                    5 => {} // a stuffed zero
                    _ => self.bits.push(0),
                }
                self.ones = 0;
            }
            if self.bits.len() > HDLC_MAX {
                self.open = false;
                self.bits.clear();
            }
        }
    }

    fn close(&mut self, out: &mut Vec<(Vec<u8>, Fcs)>) {
        if self.bits.is_empty() {
            return; // back-to-back flags
        }
        if !self.bits.len().is_multiple_of(8) || self.bits.len() < 6 * 8 {
            self.bad += 1;
            return;
        }
        let bytes: Vec<u8> = self
            .bits
            .as_chunks::<8>()
            .0
            .iter()
            .map(|c| c.iter().rev().fold(0u8, |a, &b| (a << 1) | b))
            .collect();
        if crc16_x25(&bytes) == 0xF0B8 {
            self.good += 1;
            out.push((bytes[..bytes.len() - 2].to_vec(), Fcs::Crc16));
        } else if crc32(&bytes) == 0xDEBB_20E3 {
            self.good += 1;
            out.push((bytes[..bytes.len() - 4].to_vec(), Fcs::Crc32));
        } else {
            self.bad += 1;
        }
    }
}

/// The self-synchronising scrambler that [`Descrambler::SelfSync`] undoes:
/// out = in ⊕ out[n−t₁] ⊕ out[n−t₂] …. For test signals.
pub struct SelfSyncScrambler {
    taps: &'static [usize],
    history: u32,
}

impl SelfSyncScrambler {
    pub fn new(taps: &'static [usize]) -> Self {
        SelfSyncScrambler { taps, history: 0 }
    }

    pub fn scramble(&mut self, bits: &mut [u8]) {
        for b in bits.iter_mut() {
            let mut y = *b;
            for &t in self.taps {
                y ^= ((self.history >> (t - 1)) & 1) as u8;
            }
            self.history = (self.history << 1) | y as u32;
            *b = y;
        }
    }
}

/// HDLC framing of `data` (LSB first, zero-bit stuffed, FCS-16, a flag
/// before and after): for tests and test signals.
pub fn hdlc_frame(data: &[u8], out: &mut Vec<u8>) {
    let flag = |out: &mut Vec<u8>| out.extend([0, 1, 1, 1, 1, 1, 1, 0]);
    flag(out);
    let fcs = !crc16_x25(data);
    let mut ones = 0;
    for &byte in data.iter().chain(&fcs.to_le_bytes()) {
        for i in 0..8 {
            let b = (byte >> i) & 1;
            out.push(b);
            if b == 1 {
                ones += 1;
                if ones == 5 {
                    out.push(0);
                    ones = 0;
                }
            } else {
                ones = 0;
            }
        }
    }
    flag(out);
}

/// MPEG-TS packet alignment on a bit stream, at any bit offset: found
/// where 47h starts three bytes 188 apart, then followed packet by packet
/// until the sync byte has been missing three times running.
#[derive(Clone, Default)]
pub struct TsAlign {
    bits: Vec<u8>,
    locked: bool,
    misses: u32,
    pub packets: u64,
}

const TS_BITS: usize = 188 * 8;

fn byte_at(bits: &[u8], at: usize) -> u8 {
    bits[at..at + 8].iter().fold(0u8, |a, &b| (a << 1) | b)
}

impl TsAlign {
    pub fn push(&mut self, bits: &[u8], out: &mut Vec<[u8; 188]>) {
        self.bits.extend_from_slice(bits);
        let mut at = 0;
        loop {
            if self.locked {
                if at + TS_BITS > self.bits.len() {
                    break;
                }
                if byte_at(&self.bits, at) == 0x47 {
                    self.misses = 0;
                } else {
                    self.misses += 1;
                    if self.misses >= 3 {
                        self.locked = false;
                        at += 1;
                        continue;
                    }
                }
                let mut p = [0u8; 188];
                for (i, v) in p.iter_mut().enumerate() {
                    *v = byte_at(&self.bits, at + 8 * i);
                }
                p[0] = 0x47;
                out.push(p);
                self.packets += 1;
                at += TS_BITS;
            } else {
                if at + 2 * TS_BITS + 8 > self.bits.len() {
                    break;
                }
                if (0..3).all(|k| byte_at(&self.bits, at + k * TS_BITS) == 0x47) {
                    self.locked = true;
                    self.misses = 0;
                } else {
                    at += 1;
                }
            }
        }
        self.bits.drain(..at);
    }
}

/// What the data were found to carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Hdlc,
    Ts,
    E1,
}

/// The data of one frame, made useful.
#[derive(Default)]
pub struct PayloadOut {
    /// HDLC frames that checked, without their FCS.
    pub frames: Vec<(Vec<u8>, Fcs)>,
    pub ts: Vec<[u8; 188]>,
    /// E1 frames: 32 timeslot bytes each.
    pub e1: Vec<[u8; TIMESLOTS]>,
    /// The descrambled data, MSB first, while the format is unknown or for
    /// recording.
    pub raw: Vec<u8>,
}

impl PayloadOut {
    pub fn clear(&mut self) {
        self.frames.clear();
        self.ts.clear();
        self.e1.clear();
        self.raw.clear();
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PayloadStats {
    /// Found, and how: e.g. "HDLC (FCS-16), not scrambled".
    pub found: Option<String>,
    pub hdlc_good: u64,
    pub hdlc_bad: u64,
    pub ts_packets: u64,
    /// E1 framing, when that is what the data are.
    pub e1: Option<crate::e1::E1Stats>,
    /// Frames of data looked at before deciding (or so far).
    pub probed: u64,
}

/// Frames of data gathered before probing, and the most kept.
const PROBE_MIN: usize = 4;
const PROBE_MAX: usize = 32;
/// Checked units (HDLC frames, TS packets) needed to decide.
const PROBE_UNITS: u64 = 3;

/// Finds the descrambler and format of a modem's data, then follows them.
pub struct PayloadRx {
    frame_len: usize,
    held: Vec<Vec<u8>>,
    chosen: Option<(Running, Format)>,
    hdlc: Hdlc,
    ts: TsAlign,
    e1: E1Rx,
    /// Bits not yet packed into `raw` bytes.
    raw_bits: Vec<u8>,
    pub stats: PayloadStats,
}

impl PayloadRx {
    /// `frame_len`: data bits per modem frame (additive scramblers restart
    /// each frame).
    pub fn new(frame_len: usize) -> Self {
        PayloadRx {
            frame_len,
            held: Vec::new(),
            chosen: None,
            hdlc: Hdlc::default(),
            ts: TsAlign::default(),
            e1: E1Rx::new(),
            raw_bits: Vec::new(),
            stats: PayloadStats::default(),
        }
    }

    pub fn push(&mut self, frame: &[u8], out: &mut PayloadOut) {
        if self.chosen.is_some() {
            self.follow(frame.to_vec(), out, true);
            return;
        }
        self.held.push(frame.to_vec());
        self.stats.probed += 1;
        if self.held.len() > PROBE_MAX {
            self.held.remove(0);
        }
        // Unknown so far: pass the data on as it is.
        self.pack(frame, out);
        if self.held.len() < PROBE_MIN || !self.held.len().is_multiple_of(PROBE_MIN) {
            return;
        }
        let mut best: Option<(u64, Descrambler, Format, Fcs)> = None;
        for kind in Descrambler::all() {
            let mut d = Running::new(kind, self.frame_len);
            let (mut hdlc, mut ts, mut e1) = (Hdlc::default(), TsAlign::default(), E1Rx::new());
            let (mut frames, mut pkts, mut e1f) = (Vec::new(), Vec::new(), Vec::new());
            for f in &self.held {
                let mut bits = f.clone();
                d.frame(&mut bits);
                hdlc.push(&bits, &mut frames);
                ts.push(&bits, &mut pkts);
                e1.push(&bits, &mut e1f);
            }
            let fcs = frames.first().map_or(Fcs::Crc16, |f| f.1);
            // E1 frames are counted in TS-packet-sized lots, so the formats
            // weigh alike (a TS packet is 1504 bits, an E1 frame 256).
            let e1_units = e1.stats.frames * 256 / 1504;
            for (n, fmt) in [
                (hdlc.good, Format::Hdlc),
                (ts.packets, Format::Ts),
                (e1_units, Format::E1),
            ] {
                if n >= PROBE_UNITS && best.is_none_or(|b| n > b.0) {
                    best = Some((n, kind, fmt, fcs));
                }
            }
        }
        if let Some((_, kind, fmt, fcs)) = best {
            self.stats.found = Some(format!(
                "{}, {}",
                match (fmt, fcs) {
                    (Format::Ts, _) => "MPEG-TS",
                    (Format::E1, _) => "E1 (G.704 framing)",
                    (Format::Hdlc, Fcs::Crc16) => "HDLC (FCS-16)",
                    (Format::Hdlc, Fcs::Crc32) => "HDLC (FCS-32)",
                },
                kind.describe()
            ));
            self.chosen = Some((Running::new(kind, self.frame_len), fmt));
            // Replay what was held so no frame or packet is lost; only the
            // newest frame's data go to `raw` again (the rest went out
            // already), descrambled this time.
            self.raw_bits.clear();
            out.raw.clear();
            let held = std::mem::take(&mut self.held);
            let last = held.len() - 1;
            for (i, f) in held.into_iter().enumerate() {
                self.follow(f, out, i == last);
            }
        }
    }

    fn follow(&mut self, mut bits: Vec<u8>, out: &mut PayloadOut, raw: bool) {
        let (d, fmt) = self.chosen.as_mut().expect("chosen");
        d.frame(&mut bits);
        match fmt {
            Format::Hdlc => {
                self.hdlc.push(&bits, &mut out.frames);
                self.stats.hdlc_good = self.hdlc.good;
                self.stats.hdlc_bad = self.hdlc.bad;
            }
            Format::Ts => {
                self.ts.push(&bits, &mut out.ts);
                self.stats.ts_packets = self.ts.packets;
            }
            Format::E1 => {
                self.e1.push(&bits, &mut out.e1);
                self.stats.e1 = Some(self.e1.stats.clone());
            }
        }
        if raw {
            self.pack(&bits, out);
        }
    }

    fn pack(&mut self, bits: &[u8], out: &mut PayloadOut) {
        self.raw_bits.extend_from_slice(bits);
        let whole = self.raw_bits.len() / 8 * 8;
        // `as_chunks::<8>()` views the slice as whole [u8; 8] arrays (and
        // a remainder, here empty).
        out.raw.extend(
            self.raw_bits[..whole]
                .as_chunks::<8>()
                .0
                .iter()
                .map(|c| byte_at(c, 0)),
        );
        self.raw_bits.drain(..whole);
    }

    pub fn format(&self) -> Option<Format> {
        self.chosen.as_ref().map(|c| c.1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tpc2964::tests::rng;

    /// A scrambler matching each descrambler, for tests.
    fn scramble(kind: Descrambler, frame_len: usize, bits: &[u8]) -> Vec<u8> {
        match kind {
            Descrambler::None => bits.to_vec(),
            Descrambler::SelfSync(taps) => {
                let mut b = bits.to_vec();
                SelfSyncScrambler::new(taps).scramble(&mut b);
                b
            }
            Descrambler::Additive { last, reversed } => {
                let seq = additive_sequence(last, reversed, frame_len);
                bits.chunks(frame_len)
                    .flat_map(|c| c.iter().zip(&seq).map(|(b, s)| b ^ s).collect::<Vec<_>>())
                    .collect()
            }
        }
    }

    fn hdlc_stream(n: usize, seed: u64) -> (Vec<Vec<u8>>, Vec<u8>) {
        let mut next = rng(seed);
        let mut frames = Vec::new();
        let mut bits = Vec::new();
        for _ in 0..n {
            let len = 40 + (next() % 300) as usize;
            let f: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            hdlc_frame(&f, &mut bits);
            // Idle flags between frames.
            for _ in 0..(next() % 3) {
                bits.extend([0, 1, 1, 1, 1, 1, 1, 0]);
            }
            frames.push(f);
        }
        (frames, bits)
    }

    #[test]
    fn hdlc_round_trip_with_stuffing() {
        let (frames, bits) = hdlc_stream(50, 3);
        let mut h = Hdlc::default();
        let mut out = Vec::new();
        for c in bits.chunks(777) {
            h.push(c, &mut out);
        }
        let got: Vec<Vec<u8>> = out.into_iter().map(|f| f.0).collect();
        assert_eq!(got, frames);
        assert_eq!(h.bad, 0);
    }

    #[test]
    fn finds_scrambling_and_format() {
        const L: usize = 2223;
        for kind in Descrambler::all() {
            // HDLC.
            let (frames, bits) = hdlc_stream(120, 9);
            let sent = scramble(kind, L, &bits);
            let mut rx = PayloadRx::new(L);
            let mut out = PayloadOut::default();
            let mut got = Vec::new();
            for f in sent.as_chunks::<L>().0 {
                rx.push(f, &mut out);
                got.extend(out.frames.drain(..).map(|f| f.0));
            }
            assert_eq!(rx.format(), Some(Format::Hdlc), "{kind:?}: {:?}", rx.stats);
            assert!(rx.stats.found.as_ref().unwrap().contains(&kind.describe()));
            // Every frame within the whole frames sent, bar the one cut.
            let first = frames.iter().position(|f| *f == got[0]).unwrap();
            assert!(first <= 1, "{kind:?}");
            assert!(got.len() + 3 >= frames.len(), "{kind:?}: {}", got.len());
            assert_eq!(got[..], frames[first..first + got.len()]);

            // MPEG-TS, starting at an odd bit.
            let mut next = rng(4);
            let mut bits = vec![1, 0, 1];
            for k in 0..40u8 {
                let mut p = [0u8; 188];
                p[0] = 0x47;
                p[1] = k;
                for v in &mut p[2..] {
                    *v = next() as u8;
                }
                bits.extend(
                    p.iter()
                        .flat_map(|&b| (0..8).rev().map(move |i| (b >> i) & 1)),
                );
            }
            let sent = scramble(kind, L, &bits);
            let mut rx = PayloadRx::new(L);
            let mut pkts = Vec::new();
            for f in sent.as_chunks::<L>().0 {
                rx.push(f, &mut out);
                pkts.append(&mut out.ts);
            }
            assert_eq!(rx.format(), Some(Format::Ts), "{kind:?}");
            // The first packet or two can be lost to the descrambler's start.
            assert!(pkts.len() >= 30, "{kind:?}: {}", pkts.len());
            for w in pkts.windows(2) {
                assert_eq!(w[1][1], w[0][1] + 1);
            }
        }
    }

    #[test]
    fn finds_a_scrambled_e1() {
        use crate::e1::{E1Tx, alaw_encode};
        const L: usize = 2223;
        let mut tx = E1Tx::new();
        let mut bits = vec![0, 1, 1];
        let mut next = rng(12);
        for n in 0..400 {
            let mut ts = [0u8; TIMESLOTS];
            for v in ts.iter_mut().skip(1) {
                *v = 0xD5; // idle channels
            }
            ts[7] = alaw_encode(0.3 * (n as f32 * 0.4).sin());
            ts[20] = next() as u8;
            tx.frame(&ts, &mut bits);
        }
        let kind = Descrambler::SelfSync(&[3, 20]);
        let sent = scramble(kind, L, &bits);
        let mut rx = PayloadRx::new(L);
        let mut out = PayloadOut::default();
        let mut frames = Vec::new();
        for f in sent.as_chunks::<L>().0 {
            rx.push(f, &mut out);
            frames.append(&mut out.e1);
        }
        assert_eq!(rx.format(), Some(Format::E1), "{:?}", rx.stats);
        assert!(rx.stats.found.as_ref().unwrap().contains("taps 3, 20"));
        assert!(frames.len() > 300, "{}", frames.len());
        assert!(frames.iter().all(|f| f[1] == 0xD5));
    }

    #[test]
    fn random_data_is_not_mistaken() {
        let mut next = rng(77);
        let mut rx = PayloadRx::new(2223);
        let mut out = PayloadOut::default();
        for _ in 0..40 {
            let f: Vec<u8> = (0..2223).map(|_| (next() & 1) as u8).collect();
            rx.push(&f, &mut out);
        }
        assert_eq!(rx.format(), None);
        assert_eq!(out.raw.len(), 40 * 2223 / 8);
    }
}
