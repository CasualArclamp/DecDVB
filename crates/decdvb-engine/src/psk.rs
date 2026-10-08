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
        }
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
        for &s in &self.raw {
            let y = self.pll.step(s, &self.cst.points);
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
        self.coherence > decdvb_dsp::LOCK_COHERENCE
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

#[cfg(test)]
mod tests {
    use super::*;
    use decdvb_mod::Shaper;

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
