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
//! Carrier recovery is not here yet (the remaining M1 item), so payload symbols
//! still carry the residual frequency offset. Everything above needs none: the
//! correlator and the PLS code are read differentially.

use decdvb_core::Iq;
use decdvb_dsp::{Agc, Fir, SymbolSync, rrc_taps};
use decdvb_frame::{
    PLHEADER_LEN, PlHeaderCorrelator, PlScrambler, PlsInfo, PlscDecoder, PlscDemap,
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

/// One demodulated PLFRAME.
#[derive(Debug, Clone)]
pub struct PlFrame {
    pub pls: PlsInfo,
    /// Payload after the PLHEADER, pilots included, descrambled. Not yet
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
        }
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

    /// The most recent recovered symbols (for a constellation display).
    pub fn recent_symbols(&self, n: usize) -> &[Iq] {
        &self.sym[self.sym.len().saturating_sub(n)..]
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
                        let p0 = hdr_end + 1;
                        let mut payload = self.sym[p0..p0 + pls.payload_len as usize].to_vec();
                        self.scrambler.descramble(&mut payload);
                        out.push(PlFrame { pls, payload, corr });
                        self.frames += 1;

                        let next_pls = self.decode_at(best);
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
                        self.state = State::Searching {
                            scan_from: hdr_end + 1,
                        };
                    }
                }
            }
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
}
