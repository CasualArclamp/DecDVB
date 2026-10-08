//! Generic PSK/APSK demodulator: baseband in, carrier-locked symbols and hard
//! decisions out, with no framing assumed.
//!
//! For carriers that are not DVB-S2 (or not yet decodable): DVB-S, SCPC data,
//! telemetry, anything linearly modulated. The recovered symbols go to a file
//! for offline work — e.g. hunting a frame sync word, or a Viterbi decoder.
//!
//! Without a known preamble the absolute phase is ambiguous by the
//! constellation's rotational symmetry (90° for QPSK, 45° for 8PSK, 180° for
//! BPSK), so the symbol indices written may be a fixed rotation of the
//! transmitted ones. That is inherent, not a fault; a sync word found offline
//! resolves it.

use std::collections::VecDeque;

use decdvb_core::{Iq, Modulation};
use decdvb_dsp::{Agc, CarrierPll, Fir, SymbolSync, rrc_taps};
use decdvb_fec::Constellation;
use decdvb_modem::text::{TextFinder, TextView};

/// Locked symbols kept for display.
const RECENT: usize = 3000;

/// Streaming generic demodulator.
pub struct PskDemod {
    mf: Fir,
    agc: Agc,
    sync: SymbolSync,
    pll: CarrierPll,
    cst: Constellation,
    rate: f64,
    filtered: Vec<Iq>,
    raw: Vec<Iq>,
    recent: VecDeque<Iq>,
    symbols: u64,
    mer: f32,
    coherence: f32,
    /// Mean symbol power, tracked: the AGC levels samples, not symbols, and
    /// QAM and APSK decisions need the symbols at the constellation's scale.
    power: Option<f32>,
    /// The outermost ring's points, and the radius beyond which a symbol is
    /// taken to be on it: until lock, a multi-ring constellation steers on
    /// those alone (the reduced-constellation algorithm) — full decisions
    /// on 16QAM can settle 26.6° off, where its inner and middle points
    /// trade places.
    outer: Vec<Iq>,
    outer_from: f32,
    /// Steering on full decisions (after the outer ring has locked).
    full: bool,
}

impl PskDemod {
    /// `rate` is the baseband sample rate, `rs` the symbol rate, `alpha` the
    /// roll-off; `offset_cycles` seeds the carrier loop (cycles per symbol).
    pub fn new(rate: f64, rs: f64, alpha: f64, modulation: Modulation, offset_cycles: f64) -> Self {
        let sps = rate / rs;
        PskDemod {
            mf: Fir::new(rrc_taps(sps, alpha, 12)),
            agc: Agc::new(1.0, 0.2),
            sync: SymbolSync::new(sps, 0.005, 0.02),
            pll: CarrierPll::new(0.008, offset_cycles),
            cst: Constellation::generic(modulation),
            rate,
            filtered: Vec::new(),
            raw: Vec::new(),
            recent: VecDeque::with_capacity(RECENT),
            symbols: 0,
            mer: 0.0,
            coherence: 0.0,
            power: None,
            outer: Vec::new(),
            outer_from: f32::INFINITY,
            full: false,
        }
        .with_outer_ring()
    }

    fn with_outer_ring(mut self) -> Self {
        let mut radii: Vec<f32> = self.cst.points.iter().map(|p| p.norm()).collect();
        radii.sort_by(|a, b| b.partial_cmp(a).unwrap());
        radii.dedup_by(|a, b| (*a - *b).abs() < 1e-3);
        if radii.len() > 1 {
            self.outer_from = 0.5 * (radii[0] + radii[1]);
            self.outer = self
                .cst
                .points
                .iter()
                .copied()
                .filter(|p| p.norm() > self.outer_from)
                .collect();
        }
        self
    }

    pub fn modulation(&self) -> Modulation {
        self.cst.modulation
    }

    /// Feed baseband; append (hard index, locked soft symbol) per symbol.
    pub fn process(&mut self, baseband: &[Iq], out: &mut Vec<(u8, Iq)>) {
        self.filtered.clear();
        self.mf.process(baseband, &mut self.filtered);
        self.agc.process(&mut self.filtered);
        self.raw.clear();
        self.sync.process(&self.filtered, &mut self.raw);

        let start = out.len();
        if self.power.is_none() && !self.raw.is_empty() {
            let p = self.raw.iter().map(|s| s.norm_sqr()).sum::<f32>() / self.raw.len() as f32;
            self.power = Some(p.max(1e-12));
        }
        for &s in &self.raw {
            let p = self.power.get_or_insert(1.0);
            *p += 0.0005 * (s.norm_sqr() - *p);
            let s = s / p.sqrt().max(1e-6);
            let y = if self.outer.is_empty() || self.full {
                self.pll.step(s, &self.cst.points)
            } else if s.norm() > self.outer_from {
                self.pll.step(s, &self.outer)
            } else {
                self.pll.coast(s)
            };
            out.push((self.cst.nearest(y) as u8, y));
            if self.recent.len() == RECENT {
                self.recent.pop_front();
            }
            self.recent.push_back(y);
        }
        let new = &out[start..];
        self.symbols += new.len() as u64;
        if !new.is_empty() {
            let soft: Vec<Iq> = new.iter().map(|&(_, y)| y).collect();
            let (mer, coh) = decdvb_dsp::quality(&soft, |_| &self.cst.points);
            // Smooth over blocks so the readout does not flicker.
            self.mer += 0.3 * (mer - self.mer);
            self.coherence += 0.3 * (coh - self.coherence);
            // Full decisions only once truly locked — a 16QAM false lock
            // still scores ~0.45 — and back to the outer ring if it fades.
            if !self.full && self.coherence > 0.7 {
                self.full = true;
            } else if self.full && self.coherence < 0.4 {
                self.full = false;
            }
        }
    }

    pub fn recent(&self) -> Vec<Iq> {
        self.recent.iter().copied().collect()
    }

    pub fn symbols(&self) -> u64 {
        self.symbols
    }

    pub fn mer_db(&self) -> f32 {
        self.mer
    }

    pub fn coherence(&self) -> f32 {
        self.coherence
    }

    pub fn locked(&self) -> bool {
        // A multi-ring constellation's false locks score well above what
        // a single ring's lock needs: ask more of it.
        let need = if self.outer.is_empty() {
            decdvb_dsp::LOCK_COHERENCE
        } else {
            0.6
        };
        self.coherence > need
    }

    /// Symbol rate as the timing loop tracks it.
    pub fn symbol_rate(&self) -> f64 {
        self.rate / self.sync.sps()
    }

    /// Residual carrier offset the loop is tracking, Hz.
    pub fn carrier_offset_hz(&self) -> f64 {
        self.pll.freq_cycles() * self.symbol_rate()
    }
}

/// Live text search on a generic carrier: the hard decisions read under
/// each of the constellation's phase ambiguities (and their mirror images),
/// each a bit stream for a [`TextFinder`].
pub struct TextSearch {
    finder: TextFinder,
    cst: Constellation,
    /// Turn applied before deciding, and whether to mirror (conjugate) first.
    turns: Vec<(Iq, bool)>,
    bits: Vec<Vec<u8>>,
}

impl TextSearch {
    pub fn new(modulation: Modulation) -> Self {
        let cst = Constellation::generic(modulation);
        let (steps, mirror) = match modulation {
            Modulation::Bpsk | Modulation::Pi2Bpsk => (2, false),
            Modulation::Psk8 => (8, true),
            _ => (4, true),
        };
        let mut turns = Vec::new();
        let mut names = Vec::new();
        for m in [false, true].into_iter().take(if mirror { 2 } else { 1 }) {
            for k in 0..steps {
                let a = std::f32::consts::TAU * k as f32 / steps as f32;
                turns.push((Iq::new(a.cos(), a.sin()), m));
                names.push(format!(
                    "turned {}°{}",
                    360 * k / steps,
                    if m { ", mirrored" } else { "" }
                ));
            }
        }
        TextSearch {
            finder: TextFinder::new(names),
            bits: vec![Vec::new(); turns.len()],
            cst,
            turns,
        }
    }

    pub fn push(&mut self, syms: &[(u8, Iq)]) {
        let nb = self.cst.bits() as usize;
        for b in &mut self.bits {
            b.clear();
        }
        for &(_, y) in syms {
            for (k, &(turn, mirror)) in self.turns.iter().enumerate() {
                let z = if mirror { y.conj() } else { y } * turn;
                let label = self.cst.nearest(z);
                // The label's bits, most significant first.
                self.bits[k].extend((0..nb).rev().map(|i| ((label >> i) & 1) as u8));
            }
        }
        for (k, b) in self.bits.iter().enumerate() {
            self.finder.push(k, b);
        }
    }

    pub fn view(&self) -> TextView {
        self.finder.view()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use decdvb_mod::Shaper;

    #[test]
    fn locks_16qam_without_false_lock() {
        // Full decisions on 16QAM settled 27° off (MER 11 dB); acquiring on
        // the corners alone does not.
        let cst = Constellation::qam16();
        let mut s = 0x1234u64;
        let mut next = move || {
            s ^= s >> 12;
            s ^= s << 25;
            s ^= s >> 27;
            s.wrapping_mul(0x2545_F491_4F6C_DD1D)
        };
        let syms: Vec<Iq> = (0..40_000)
            .map(|_| cst.map((next() >> 60) as usize))
            .collect();
        let mut sh = Shaper::new(4, 0.25, 16);
        let mut x = Vec::new();
        sh.process(&syms, &mut x);
        for (off, seed) in [(0.0, 0.0), (0.004, 0.0036)] {
            let xx: Vec<Iq> = x
                .iter()
                .enumerate()
                .map(|(n, &v)| {
                    let ph = std::f64::consts::TAU * off * n as f64 / 4.0 + 1.1;
                    v * Iq::new(ph.cos() as f32, ph.sin() as f32)
                })
                .collect();
            let mut d = PskDemod::new(4.0, 1.0, 0.25, Modulation::Qam16, seed);
            let mut out = Vec::new();
            for c in xx.chunks(9_999) {
                d.process(c, &mut out);
            }
            assert!(
                d.locked() && d.mer_db() > 25.0,
                "offset {off}: MER {}",
                d.mer_db()
            );
        }
    }

    #[test]
    fn recovers_8psk_symbols_through_offset_and_noise() {
        let cst = Constellation::psk8();
        let mut s = 0x1234u64;
        let mut next = move || {
            s ^= s >> 12;
            s ^= s << 25;
            s ^= s >> 27;
            s.wrapping_mul(0x2545_F491_4F6C_DD1D)
        };
        let idx: Vec<usize> = (0..40_000).map(|_| (next() >> 61) as usize).collect();
        let syms: Vec<Iq> = idx.iter().map(|&i| cst.map(i)).collect();
        let mut sh = Shaper::new(4, 0.25, 16);
        let mut x = Vec::new();
        sh.process(&syms, &mut x);
        // 0.4 % of the symbol rate off, seeded 10 % wrong.
        let x: Vec<Iq> = x
            .iter()
            .enumerate()
            .map(|(n, &v)| {
                let ph = std::f64::consts::TAU * 0.004 * n as f64 / 4.0 + 1.1;
                v * Iq::new(ph.cos() as f32, ph.sin() as f32)
            })
            .collect();

        let mut d = PskDemod::new(4.0, 1.0, 0.25, Modulation::Psk8, 0.0036);
        let mut out = Vec::new();
        for c in x.chunks(9_999) {
            d.process(c, &mut out);
        }
        assert!(d.locked(), "coherence {}", d.coherence());
        assert!(d.mer_db() > 20.0, "MER {}", d.mer_db());
        assert!((d.carrier_offset_hz() - 0.004).abs() < 1e-4);

        // After pull-in the hard decisions equal the sent symbols up to one
        // fixed rotation of the 8PSK constellation (the phase ambiguity).
        let got: Vec<u8> = out.iter().map(|&(i, _)| i).collect();
        let tail = got.len() - 5_000;
        let rotate = |i: usize, k: usize| {
            // Rotate point i by k·45° and find its index.
            let a = cst.points[i]
                * Iq::new(
                    (k as f32 * std::f32::consts::FRAC_PI_4).cos(),
                    (k as f32 * std::f32::consts::FRAC_PI_4).sin(),
                );
            cst.nearest(a) as u8
        };
        // Align by searching the small lag the filters introduce, and the
        // rotation.
        let best = (0..30)
            .flat_map(|lag| (0..8).map(move |k| (lag, k)))
            .map(|(lag, k)| {
                let agree = (tail..got.len())
                    .filter(|&n| n >= lag && rotate(idx[n - lag], k) == got[n])
                    .count();
                agree as f64 / (got.len() - tail) as f64
            })
            .fold(0.0f64, f64::max);
        assert!(best > 0.99, "only {:.3} of symbols agree", best);
    }
}
