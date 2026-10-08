//! Streaming DVB-S2 physical-layer demodulator: VFO baseband in, PLFRAMEs out.
//!
//! matched filter → AGC → Gardner timing → PLHEADER correlation → frame lock →
//! PLS decode → payload descrambling.
//!
//! **Frame lock.** A PLFRAME is only emitted once the *next* PLHEADER has been
//! found exactly where this frame's own PLS code said the frame would end. That
//! one check validates both the PLS decode and the frame length, which is what
//! makes ACM safe to follow: a mis-decoded MODCOD predicts the wrong length and
//! the next header is not there. A miss drops back to searching, which is cheap
//! because the correlation for every symbol is already computed — re-acquisition
//! happens on the very next good header.
//!
//! **Carrier recovery** needs none of the above (the correlator and the PLS
//! code are read differentially), so it runs over each frame as the frame is
//! emitted, in stream order: data-aided over the PLHEADER and the pilot blocks
//! (known symbols, so the absolute phase is resolved with no 90° ambiguity)
//! and decision-directed over the data against the frame's own MODCOD
//! constellation, so an ACM carrier stays locked as QPSK, 8PSK and APSK frames
//! alternate. At (re)acquisition the loop is seeded from the two headers that
//! confirmed the first frame: with their modulation removed they are a pure
//! tone at the carrier offset.

use std::collections::{BTreeMap, VecDeque};
use std::f64::consts::TAU;

use decdvb_core::{FecFrame, Iq, Modulation, s2_modcod};
use decdvb_dsp::{Agc, CarrierPll, Fir, LOCK_COHERENCE, SymbolSync, rrc_taps};
use decdvb_fec::Constellation;
use decdvb_frame::pi2bpsk::map_bpsk;
use decdvb_frame::{
    PILOT_BLK_LEN, PLHEADER_LEN, PlHeaderCorrelator, PlScrambler, PlsInfo, PlscDecoder, PlscDemap,
    PlscEncoder, SLOT_LEN, SLOTS_PER_PILOT_BLK, SOF_BIG_ENDIAN, SOF_LEN,
};

/// Correlation needed to start tracking a header from scratch.
const ACQUIRE_THRESHOLD: f32 = 0.5;
/// Correlation that confirms a header at its predicted position; lower than
/// the acquisition threshold because the position is already known.
const CONFIRM_THRESHOLD: f32 = 0.3;
/// How far a confirming header may be from prediction (timing slips).
const TOLERANCE: usize = 2;
/// Symbols either side a candidate must beat to count as a peak.
const PEAK_HALF_WIDTH: usize = 3;
/// Carrier loop noise bandwidth, normalised to the symbol rate.
const CARRIER_BN: f64 = 0.01;
/// Carrier-locked data symbols kept for display.
const RECENT: usize = 3000;
/// Pilots and dummy-frame payload: `(1 + j)/sqrt(2)` before scrambling
/// (§5.5.3).
const PILOT: Iq = Iq::new(
    std::f32::consts::FRAC_1_SQRT_2,
    std::f32::consts::FRAC_1_SQRT_2,
);
/// Data symbols before each pilot block, and the block-to-block period.
const PILOT_AFTER: usize = SLOTS_PER_PILOT_BLK * SLOT_LEN;
const PILOT_PERIOD: usize = PILOT_AFTER + PILOT_BLK_LEN;
/// How far an outside frequency hint may be from the headers' own estimate
/// and still be used, cycles per symbol: far past that estimate's noise, so a
/// stale hint is caught but a good one is kept at low SNR.
const SEED_TRUST: f64 = 0.02;

/// One demodulated PLFRAME.
#[derive(Debug, Clone)]
pub struct PlFrame {
    pub pls: PlsInfo,
    /// Payload after the PLHEADER, pilots included, descrambled and
    /// carrier-corrected.
    pub payload: Vec<Iq>,
    /// Header correlation, 0..1.
    pub corr: f32,
}

/// Lock state, for display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockState {
    /// Looking for a PLHEADER.
    Searching,
    /// Found one; waiting to confirm the next on the predicted grid.
    Found,
    /// Consecutive headers confirmed on the grid.
    Locked,
}

enum State {
    Searching {
        scan_from: usize,
    },
    Tracking {
        /// Index (in `sym`) of the current header's last symbol.
        hdr_end: usize,
        pls: PlsInfo,
        corr: f32,
        confirmed: u32,
    },
}

/// Streaming PL demodulator.
pub struct Demod {
    rate: f64,
    mf: Fir,
    agc: Agc,
    sync: SymbolSync,
    corr: PlHeaderCorrelator,
    dec: PlscDecoder,
    scrambler: PlScrambler,
    /// Recovered symbols and the header correlation ending at each.
    sym: Vec<Iq>,
    metric: Vec<f32>,
    state: State,
    // Scratch.
    filtered: Vec<Iq>,
    new_sym: Vec<Iq>,
    // Counters.
    frames: u64,
    lost: u64,
    // Carrier recovery.
    plsc_enc: PlscEncoder,
    pll: CarrierPll,
    /// The loop has been seeded since the last acquisition.
    pll_live: bool,
    /// Frequency hint, cycles per symbol: from outside, then the last value
    /// the loop held while locked.
    seed: Option<f64>,
    /// Constellation per MODCOD and frame size (the PLS code less its pilot
    /// bit).
    csts: BTreeMap<u8, Constellation>,
    hdr_ref: [Iq; PLHEADER_LEN],
    data: Vec<Iq>,
    recent: VecDeque<Iq>,
    mer: f32,
    coherence: f32,
    fresh: bool,
    modulation: Option<Modulation>,
}

impl Demod {
    /// Demodulate baseband at `rate` carrying a carrier of symbol rate `rs` and
    /// roll-off `alpha`, PL-scrambled with `gold_code`.
    ///
    /// # Panics
    /// If `rate / rs < 2`.
    pub fn new(rate: f64, rs: f64, alpha: f64, gold_code: u32) -> Self {
        let sps = rate / rs;
        Demod {
            rate,
            mf: Fir::new(rrc_taps(sps, alpha, 12)),
            agc: Agc::new(1.0, 0.2),
            sync: SymbolSync::new(sps, 0.005, 0.02),
            corr: PlHeaderCorrelator::new(),
            dec: PlscDecoder::new(),
            scrambler: PlScrambler::new(gold_code),
            sym: Vec::new(),
            metric: Vec::new(),
            state: State::Searching { scan_from: 0 },
            filtered: Vec::new(),
            new_sym: Vec::new(),
            frames: 0,
            lost: 0,
            plsc_enc: PlscEncoder::new(),
            pll: CarrierPll::new(CARRIER_BN, 0.0),
            pll_live: false,
            seed: None,
            csts: BTreeMap::new(),
            hdr_ref: [Iq::new(0.0, 0.0); PLHEADER_LEN],
            data: Vec::new(),
            recent: VecDeque::with_capacity(RECENT),
            mer: 0.0,
            coherence: 0.0,
            fresh: true,
            modulation: None,
        }
    }

    /// Hint the carrier offset, cycles per symbol (e.g. Identify's, averaged
    /// over many headers). Used at acquisition if the headers agree with it.
    pub fn with_carrier_offset(mut self, cycles: f64) -> Self {
        self.seed = Some(cycles);
        self
    }

    /// The carrier loop is running (frames are flowing since acquisition).
    pub fn carrier_running(&self) -> bool {
        self.pll_live
    }

    /// The carrier loop is running and the data symbols sit on their points.
    pub fn carrier_locked(&self) -> bool {
        self.pll_live && self.coherence > LOCK_COHERENCE
    }

    /// MER of the data symbols, dB, smoothed over frames.
    pub fn mer_db(&self) -> f32 {
        self.mer
    }

    /// Residual carrier offset the loop is tracking, Hz.
    pub fn carrier_offset_hz(&self) -> f64 {
        self.pll.freq_cycles() * self.symbol_rate()
    }

    /// Modulation of the last frame that carried data.
    pub fn modulation(&self) -> Option<Modulation> {
        self.modulation
    }

    pub fn lock_state(&self) -> LockState {
        match self.state {
            State::Searching { .. } => LockState::Searching,
            State::Tracking { confirmed: 0, .. } => LockState::Found,
            State::Tracking { .. } => LockState::Locked,
        }
    }

    /// Symbol rate as the timing loop currently tracks it.
    pub fn symbol_rate(&self) -> f64 {
        self.rate / self.sync.sps()
    }

    /// Frames emitted so far.
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Times lock has been lost.
    pub fn losses(&self) -> u64 {
        self.lost
    }

    /// The most recent symbols for a constellation display: carrier-locked
    /// data symbols while frames flow, else the raw recovered symbols.
    pub fn recent_symbols(&self, n: usize) -> Vec<Iq> {
        if self.pll_live && !self.recent.is_empty() {
            let skip = self.recent.len().saturating_sub(n);
            self.recent.iter().skip(skip).copied().collect()
        } else {
            self.sym[self.sym.len().saturating_sub(n)..].to_vec()
        }
    }

    /// Feed baseband; completed, grid-confirmed frames are appended to `out`.
    pub fn process(&mut self, baseband: &[Iq], out: &mut Vec<PlFrame>) {
        self.filtered.clear();
        self.mf.process(baseband, &mut self.filtered);
        self.agc.process(&mut self.filtered);
        self.new_sym.clear();
        self.sync.process(&self.filtered, &mut self.new_sym);

        for &s in &self.new_sym {
            self.sym.push(s);
            self.metric.push(self.corr.push(s).unwrap_or(0.0));
        }
        self.advance(out);
        self.trim();
    }

    fn decode_at(&mut self, hdr_end: usize) -> PlsInfo {
        // The last SOF symbol plus the 64 PLS symbols: 65 ending at `hdr_end`.
        self.dec
            .decode(&self.sym[hdr_end - 64..=hdr_end], PlscDemap::Differential)
    }

    fn advance(&mut self, out: &mut Vec<PlFrame>) {
        loop {
            let len = self.sym.len();
            match self.state {
                State::Searching { scan_from } => {
                    // A peak needs a full PLHEADER behind it and a few symbols
                    // after it to prove it is the maximum.
                    let start = scan_from.max(PLHEADER_LEN);
                    let end = len.saturating_sub(PEAK_HALF_WIDTH);
                    let found = (start..end).find(|&j| {
                        let m = self.metric[j];
                        m > ACQUIRE_THRESHOLD
                            && (j - PEAK_HALF_WIDTH..=j + PEAK_HALF_WIDTH)
                                .all(|k| k == j || self.metric[k] < m)
                    });
                    match found {
                        Some(j) => {
                            let pls = self.decode_at(j);
                            self.state = State::Tracking {
                                hdr_end: j,
                                pls,
                                corr: self.metric[j],
                                confirmed: 0,
                            };
                        }
                        None => {
                            self.state = State::Searching { scan_from: end };
                            return;
                        }
                    }
                }
                State::Tracking {
                    hdr_end,
                    pls,
                    corr,
                    confirmed,
                } => {
                    let next = hdr_end + pls.plframe_len as usize;
                    if next + TOLERANCE >= len {
                        return; // wait for the rest of the frame
                    }
                    // Best correlation around where the next header should end.
                    let (best, best_m) = (next - TOLERANCE..=next + TOLERANCE)
                        .map(|k| (k, self.metric[k]))
                        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
                        .unwrap();

                    if best_m > CONFIRM_THRESHOLD {
                        // The grid held: this frame is real. Emit it.
                        let next_pls = self.decode_at(best);
                        let p0 = hdr_end + 1;
                        let mut payload = self.sym[p0..p0 + pls.payload_len as usize].to_vec();
                        self.scrambler.descramble(&mut payload);
                        self.recover_carrier(hdr_end, pls, &mut payload, best, next_pls);
                        out.push(PlFrame { pls, payload, corr });
                        self.frames += 1;

                        self.state = State::Tracking {
                            hdr_end: best,
                            pls: next_pls,
                            corr: best_m,
                            confirmed: confirmed + 1,
                        };
                    } else {
                        if confirmed > 0 {
                            self.lost += 1;
                        }
                        // Remember where the carrier was for re-acquisition.
                        if self.carrier_locked() {
                            self.seed = Some(self.pll.freq_cycles());
                        }
                        self.pll_live = false;
                        self.state = State::Searching {
                            scan_from: hdr_end + 1,
                        };
                    }
                }
            }
        }
    }

    /// The 90 PLHEADER symbols as sent, for a PLS code, into `hdr_ref`.
    fn header_reference(&mut self, plsc: u8) {
        map_bpsk(SOF_BIG_ENDIAN, &mut self.hdr_ref[..SOF_LEN], SOF_LEN);
        self.plsc_enc.encode(plsc, &mut self.hdr_ref[SOF_LEN..]);
    }

    /// Seed the loop from two confirmed headers (first symbols at `h0`, `h1`):
    /// with their modulation removed they are a pure tone at the offset.
    fn acquire(&mut self, h0: usize, plsc0: u8, h1: usize, plsc1: u8) {
        let mut z = [[Iq::new(0.0, 0.0); PLHEADER_LEN]; 2];
        for (zk, (h, plsc)) in z.iter_mut().zip([(h0, plsc0), (h1, plsc1)]) {
            self.header_reference(plsc);
            for (n, v) in zk.iter_mut().enumerate() {
                *v = self.sym[h + n] * self.hdr_ref[n].conj();
            }
        }
        let f_hdr = tone_freq(&z);
        let f = match self.seed {
            Some(s) if (s - f_hdr).abs() < SEED_TRUST => s,
            _ => f_hdr,
        };
        // Phase at the first header's first symbol.
        let acc: Iq = z[0]
            .iter()
            .enumerate()
            .map(|(n, &v)| {
                let ph = -TAU * f * n as f64;
                v * Iq::new(ph.cos() as f32, ph.sin() as f32)
            })
            .sum();
        self.pll.set_freq_cycles(f);
        self.pll.set_phase(acc.arg() as f64);
        self.pll_live = true;
        self.fresh = true;
    }

    /// Take the carrier off one frame: its header (ending at `hdr_end`) then
    /// its descrambled `payload`, in place. `next_end`/`next_pls` are the
    /// header that confirmed it, used to seed the loop at acquisition.
    fn recover_carrier(
        &mut self,
        hdr_end: usize,
        pls: PlsInfo,
        payload: &mut [Iq],
        next_end: usize,
        next_pls: PlsInfo,
    ) {
        let h0 = hdr_end + 1 - PLHEADER_LEN;
        if !self.pll_live {
            self.acquire(h0, pls.plsc, next_end + 1 - PLHEADER_LEN, next_pls.plsc);
        }
        self.header_reference(pls.plsc);
        for n in 0..PLHEADER_LEN {
            self.pll.step_known(self.sym[h0 + n], self.hdr_ref[n]);
        }

        let cst = if pls.dummy_frame {
            None
        } else {
            Some(
                &*self
                    .csts
                    .entry(pls.plsc >> 1)
                    .or_insert_with(|| frame_constellation(pls)),
            )
        };
        self.data.clear();
        for (i, x) in payload.iter_mut().enumerate() {
            let pilot = pls.has_pilots && i % PILOT_PERIOD >= PILOT_AFTER;
            *x = match cst {
                Some(c) if !pilot => {
                    let y = self.pll.step(*x, &c.points);
                    self.data.push(y);
                    y
                }
                _ => self.pll.step_known(*x, PILOT),
            };
        }

        let Some(c) = cst else { return };
        let (mer, coh) = decdvb_dsp::quality(&self.data, |_| &c.points);
        if self.fresh {
            (self.mer, self.coherence) = (mer, coh);
            self.fresh = false;
        } else {
            self.mer += 0.3 * (mer - self.mer);
            self.coherence += 0.3 * (coh - self.coherence);
        }
        self.modulation = Some(c.modulation);
        for &y in self
            .data
            .iter()
            .skip(self.data.len().saturating_sub(RECENT))
        {
            if self.recent.len() == RECENT {
                self.recent.pop_front();
            }
            self.recent.push_back(y);
        }
    }

    /// Drop symbols nothing will look at again.
    fn trim(&mut self) {
        let keep_from = match self.state {
            State::Searching { scan_from } => scan_from.saturating_sub(PLHEADER_LEN + 8),
            State::Tracking { hdr_end, .. } => hdr_end.saturating_sub(PLHEADER_LEN + 8),
        };
        // Trim in large steps; shifting a big Vec every block would cost more
        // than it saves.
        if keep_from > 200_000 {
            self.sym.drain(..keep_from);
            self.metric.drain(..keep_from);
            match &mut self.state {
                State::Searching { scan_from } => *scan_from -= keep_from,
                State::Tracking { hdr_end, .. } => *hdr_end -= keep_from,
            }
        }
    }
}

/// The constellation a frame's data is decided against; QPSK for MODCODs
/// outside the S2 table (its decisions still track the 4-fold symmetry every
/// S2 constellation has).
fn frame_constellation(pls: PlsInfo) -> Constellation {
    let size = if pls.short_fecframe {
        FecFrame::Short
    } else {
        FecFrame::Normal
    };
    s2_modcod(pls.modcod, size)
        .and_then(|mc| Constellation::for_modcod(mc.modulation, mc.rate))
        .unwrap_or_else(Constellation::qpsk)
}

/// Frequency of a pure tone in noise, cycles per symbol, from blocks of it: a
/// lag-1 differential for an unambiguous coarse value, refined at lags 8 and
/// 32 (each lag's phase is read only after the coarser estimate is removed,
/// so it never wraps).
fn tone_freq(blocks: &[[Iq; PLHEADER_LEN]]) -> f64 {
    let mut f = 0.0;
    for lag in [1usize, 8, 32] {
        let mut acc = Iq::new(0.0, 0.0);
        for b in blocks {
            for n in 0..PLHEADER_LEN - lag {
                acc += b[n + lag] * b[n].conj();
            }
        }
        let w = -TAU * f * lag as f64;
        let resid = (acc * Iq::new(w.cos() as f32, w.sin() as f32)).arg() as f64;
        f += resid / (TAU * lag as f64);
    }
    f
}

#[cfg(test)]
mod tests {
    use super::*;
    use decdvb_mod::{FrameSpec, PlFramer, Shaper};

    fn signal(schedule: &[FrameSpec], n_sym: usize, sps: usize, alpha: f64, seed: u64) -> Vec<Iq> {
        let syms = PlFramer::new(0, seed).build_schedule(schedule, n_sym);
        let mut sh = Shaper::new(sps, alpha, 16);
        let mut x = Vec::new();
        sh.process(&syms, &mut x);
        x
    }

    #[test]
    fn follows_an_acm_sequence_frame_by_frame() {
        let schedule = [
            FrameSpec::new(4, false, true),
            FrameSpec::new(14, false, true),
            FrameSpec::new(20, true, false),
            FrameSpec::new(0, false, false),
            FrameSpec::new(9, true, true),
        ];
        let x = signal(&schedule, 250_000, 4, 0.25, 4);

        let mut d = Demod::new(4.0, 1.0, 0.25, 0);
        let mut frames = Vec::new();
        // Feed in uneven blocks, as a VFO would.
        for chunk in x.chunks(12_345) {
            d.process(chunk, &mut frames);
        }

        assert!(frames.len() >= 8, "only {} frames", frames.len());
        assert_eq!(d.lock_state(), LockState::Locked);
        assert_eq!(d.losses(), 0);

        // Frames must follow the schedule in order, from wherever lock began.
        let first = schedule
            .iter()
            .position(|s| s.modcod == frames[0].pls.modcod)
            .unwrap();
        for (k, f) in frames.iter().enumerate() {
            let want = schedule[(first + k) % schedule.len()];
            assert_eq!(f.pls.modcod, want.modcod, "frame {k}");
            assert_eq!(f.pls.short_fecframe, want.short_fecframe, "frame {k}");
            assert_eq!(f.payload.len(), f.pls.payload_len as usize);
        }
    }

    #[test]
    fn descrambled_pilots_are_constant() {
        // With pilots, the descrambled pilot blocks must all be the same symbol
        // (up to the residual carrier phase) — a check that descrambling lines
        // up with the frame start exactly.
        let x = signal(&[FrameSpec::new(4, false, true)], 120_000, 4, 0.2, 7);
        let mut d = Demod::new(4.0, 1.0, 0.2, 0);
        let mut frames = Vec::new();
        d.process(&x, &mut frames);
        let f = frames.last().expect("no frames");

        // First pilot block: after 16 slots of 90 data symbols.
        let p = &f.payload[16 * 90..16 * 90 + 36];
        let mean = p.iter().sum::<Iq>() / 36.0;
        let spread = p.iter().map(|s| (s - mean).norm()).fold(0.0f32, f32::max);
        assert!(mean.norm() > 0.8, "pilot mean {mean}");
        assert!(spread < 0.15, "pilots not constant: spread {spread}");
    }

    #[test]
    fn recovers_after_a_dropout() {
        let x = signal(&[FrameSpec::new(4, true, false)], 200_000, 4, 0.35, 8);
        let mut d = Demod::new(4.0, 1.0, 0.35, 0);
        let mut frames = Vec::new();

        let cut = x.len() / 2;
        d.process(&x[..cut], &mut frames);
        let before = frames.len();
        // 30 000 samples of silence where the signal should be.
        d.process(&vec![Iq::new(0.0, 0.0); 30_000], &mut frames);
        d.process(&x[cut + 30_000..], &mut frames);

        assert!(before >= 3);
        assert!(d.losses() >= 1, "the dropout should have broken lock");
        assert!(frames.len() > before + 3, "did not re-acquire");
        assert_eq!(d.lock_state(), LockState::Locked);
    }

    #[test]
    fn noise_alone_never_produces_frames() {
        let mut s = 0x1234_5678u64;
        let x: Vec<Iq> = (0..400_000)
            .map(|_| {
                s ^= s >> 12;
                s ^= s << 25;
                s ^= s >> 27;
                let v = s.wrapping_mul(0x2545_F491_4F6C_DD1D);
                Iq::new(
                    ((v >> 40) as f32 / (1u32 << 24) as f32) - 0.5,
                    (((v >> 16) & 0xFF_FFFF) as f32 / (1u32 << 24) as f32) - 0.5,
                )
            })
            .collect();
        let mut d = Demod::new(4.0, 1.0, 0.35, 0);
        let mut frames = Vec::new();
        d.process(&x, &mut frames);
        assert!(
            frames.is_empty(),
            "{} false frames from noise",
            frames.len()
        );
    }

    /// `x` offset by `cycles` per sample, rotated by `phase`, plus complex
    /// Gaussian noise of power `10^(-snr_db/10)` per sample (unit signal).
    fn impair(x: &[Iq], cycles: f64, phase: f64, snr_db: f64, seed: u64) -> Vec<Iq> {
        let mut s = seed | 1;
        let mut uniform = move || {
            s ^= s >> 12;
            s ^= s << 25;
            s ^= s >> 27;
            (s.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
        };
        let sigma = 10f64.powf(-snr_db / 20.0) * std::f64::consts::FRAC_1_SQRT_2;
        x.iter()
            .enumerate()
            .map(|(n, &v)| {
                let ph = TAU * cycles * n as f64 + phase;
                let r = (-2.0 * uniform().max(1e-300).ln()).sqrt() * sigma;
                let t = TAU * uniform();
                v * Iq::new(ph.cos() as f32, ph.sin() as f32)
                    + Iq::new((r * t.cos()) as f32, (r * t.sin()) as f32)
            })
            .collect()
    }

    #[test]
    fn locks_the_carrier_of_an_acm_sequence() {
        // QPSK, 8PSK, short 16APSK, dummy and short QPSK frames, 0.3 % of the
        // symbol rate off and rotated: every data frame must come out on its
        // own constellation, and the pilots on their true phase (the header
        // resolves the 90° ambiguity a blind loop would be left with).
        let schedule = [
            FrameSpec::new(4, false, true),
            FrameSpec::new(14, false, true),
            FrameSpec::new(20, true, false),
            FrameSpec::new(0, false, false),
            FrameSpec::new(9, true, true),
        ];
        let x = signal(&schedule, 300_000, 4, 0.25, 11);
        let x = impair(&x, 0.003 / 4.0, 2.2, 20.0, 12);

        let mut d = Demod::new(4.0, 1.0, 0.25, 0);
        let mut frames = Vec::new();
        for chunk in x.chunks(20_000) {
            d.process(chunk, &mut frames);
        }
        assert!(frames.len() >= 8, "only {} frames", frames.len());
        assert_eq!(d.losses(), 0);
        assert!(d.carrier_locked(), "MER {:.1} dB", d.mer_db());
        assert!(d.mer_db() > 14.0, "MER {:.1} dB", d.mer_db());
        let f = d.carrier_offset_hz();
        assert!((f - 0.003).abs() < 2e-4, "offset {f}");

        // After the first frame, every pilot block sits on (1 + j)/sqrt(2).
        for fr in frames.iter().skip(1).filter(|f| f.pls.has_pilots) {
            let p = &fr.payload[PILOT_AFTER..PILOT_PERIOD];
            let mean = p.iter().sum::<Iq>() / PILOT_BLK_LEN as f32;
            assert!(
                (mean - PILOT).norm() < 0.15,
                "MODCOD {} pilots at {mean}",
                fr.pls.modcod
            );
        }
        // And the data of each frame on its own constellation.
        for fr in frames.iter().skip(1).filter(|f| !f.pls.dummy_frame) {
            let c = frame_constellation(fr.pls);
            let data: Vec<Iq> = fr
                .payload
                .iter()
                .enumerate()
                .filter(|(i, _)| !fr.pls.has_pilots || i % PILOT_PERIOD < PILOT_AFTER)
                .map(|(_, &v)| v)
                .collect();
            let mer = decdvb_dsp::mer_db(&data, &c.points);
            assert!(mer > 14.0, "MODCOD {}: MER {mer:.1} dB", fr.pls.modcod);
        }
    }

    #[test]
    fn a_wrong_seed_is_overruled_by_the_headers() {
        let x = signal(&[FrameSpec::new(4, false, true)], 150_000, 4, 0.2, 13);
        let x = impair(&x, -0.01 / 4.0, 0.3, 20.0, 14);
        let mut d = Demod::new(4.0, 1.0, 0.2, 0).with_carrier_offset(0.04);
        let mut frames = Vec::new();
        d.process(&x, &mut frames);
        assert!(d.carrier_locked(), "MER {:.1} dB", d.mer_db());
        assert!((d.carrier_offset_hz() + 0.01).abs() < 2e-4);
    }

    #[test]
    fn tone_freq_is_unambiguous_and_fine() {
        for f in [-0.2, -0.013, 0.0, 0.0047, 0.11] {
            let mut b = [[Iq::new(0.0, 0.0); PLHEADER_LEN]; 2];
            for (k, blk) in b.iter_mut().enumerate() {
                for (n, v) in blk.iter_mut().enumerate() {
                    let ph = TAU * f * (n + 1000 * k) as f64 + 0.7 * k as f64;
                    *v = Iq::new(ph.cos() as f32, ph.sin() as f32);
                }
            }
            assert!((tone_freq(&b) - f).abs() < 1e-6, "{f}");
        }
    }
}
