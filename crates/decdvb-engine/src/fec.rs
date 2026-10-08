//! The FEC stage of a DVB-S2 VFO: demodulated PLFRAMEs in, BBFRAMEs out.
//!
//! pilots out → scale by the measured gain → max-log LLRs, de-interleaved →
//! LDPC (layered min-sum) → BCH → BB descrambling → BBHEADER (CRC-8).
//!
//! It runs on its own thread per VFO (`FecWorker`), fed through a bounded
//! queue: the demodulator must keep real time, and when decoding cannot,
//! frames are dropped and counted rather than stalling it.

use std::collections::{BTreeMap, HashMap};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

use decdvb_core::{FecFrame, Iq, s2_modcod};
use decdvb_fec::demap::{demap_llr, quantize};
use decdvb_fec::{Bch, BchError, Constellation, DecodeOutcome, FecParams, LdpcCode, LdpcDecoder};
use decdvb_frame::{BbHeader, BbHeaderError, PlsInfo, bb_scramble};

use crate::demod::{PILOT_AFTER, PILOT_PERIOD, PlFrame};

/// LLR quantization: steps per LLR unit (the decoder is happy from 2 to 8).
const LLR_SCALE: f32 = 4.0;
/// LDPC iteration budget per frame.
const MAX_ITERATIONS: usize = 50;
/// Frames queued for the FEC thread before new ones are dropped.
const QUEUE: usize = 64;

/// One decoded BBFRAME.
#[derive(Debug, Clone)]
pub struct BbFrame {
    pub pls: PlsInfo,
    /// The descrambled BBFRAME: header, data field, padding (K_bch bits).
    pub bytes: Vec<u8>,
    pub header: Result<BbHeader, BbHeaderError>,
    pub ldpc: DecodeOutcome,
    /// Bits BCH corrected, or why it could not.
    pub bch: Result<usize, BchError>,
    pub es_n0_db: f32,
}

impl BbFrame {
    /// BCH accepted the codeword and the BBHEADER's CRC-8 checks.
    pub fn ok(&self) -> bool {
        self.bch.is_ok() && self.header.is_ok()
    }
}

struct Code {
    params: FecParams,
    cst: Constellation,
    ldpc: LdpcDecoder,
    bch: Bch,
}

/// Decodes PLFRAMEs; holds a decoder per code met so far.
#[derive(Default)]
pub struct FecDecoder {
    codes: HashMap<(bool, u8), Option<Code>>,
    data: Vec<Iq>,
    llr: Vec<f32>,
    quantized: Vec<i8>,
    info: Vec<u8>,
}

impl FecDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decode one frame; `None` for a dummy frame or a MODCOD with no S2 code
    /// (short 9/10, or the S2X ones until M3).
    pub fn decode(&mut self, f: &PlFrame) -> Option<BbFrame> {
        if f.pls.dummy_frame {
            return None;
        }
        let FecDecoder {
            codes,
            data,
            llr,
            quantized,
            info,
        } = self;
        let code = codes
            .entry((f.pls.short_fecframe, f.pls.modcod))
            .or_insert_with(|| build_code(f.pls))
            .as_mut()?;
        let p = code.params;

        // Data symbols only, on the constellation's unit-power scale.
        let inv = 1.0 / f.gain.max(1e-6);
        data.clear();
        data.extend(
            f.payload
                .iter()
                .enumerate()
                .filter(|(i, _)| !f.pls.has_pilots || i % PILOT_PERIOD < PILOT_AFTER)
                .map(|(_, &y)| y * inv),
        );
        let n = p.n_ldpc;
        if data.len() * code.cst.bits() as usize != n {
            return None;
        }
        llr.resize(n, 0.0);
        quantized.resize(n, 0);
        info.resize(p.n_bch / 8, 0);

        demap_llr(data, &code.cst, p.rate, f.noise_var * inv * inv, llr);
        quantize(llr, LLR_SCALE, quantized);
        let ldpc = code.ldpc.decode(quantized, info, MAX_ITERATIONS);

        let mut bytes = info.clone();
        let bch = code.bch.decode(&mut bytes);
        bytes.truncate(p.bbframe_bytes());
        bb_scramble(&mut bytes);
        let header = BbHeader::parse(&bytes);
        Some(BbFrame {
            pls: f.pls,
            bytes,
            header,
            ldpc,
            bch,
            es_n0_db: f.es_n0_db(),
        })
    }
}

fn build_code(pls: PlsInfo) -> Option<Code> {
    let size = if pls.short_fecframe {
        FecFrame::Short
    } else {
        FecFrame::Normal
    };
    let mc = s2_modcod(pls.modcod, size)?;
    let params = FecParams::new(size, mc.rate)?;
    Some(Code {
        params,
        cst: Constellation::for_modcod(mc.modulation, mc.rate)?,
        ldpc: LdpcDecoder::new(LdpcCode::new(params.ldpc_table())),
        bch: Bch::new(size, params.t, params.n_bch),
    })
}

/// What a VFO's FEC has done, for display.
#[derive(Debug, Clone, Default)]
pub struct FecStats {
    /// Data frames decoded (dummy frames excluded).
    pub frames: u64,
    /// BCH and the BBHEADER CRC both passed.
    pub ok: u64,
    /// BCH could not correct (LDPC left too many errors).
    pub bch_failed: u64,
    /// BCH passed but the BBHEADER CRC did not.
    pub crc_failed: u64,
    /// LDPC ended with checks unsatisfied (BCH may still have rescued it).
    pub ldpc_unconverged: u64,
    /// Frames dropped because decoding fell behind.
    pub dropped: u64,
    /// LDPC iterations, summed over `frames`.
    pub iterations: u64,
    /// Bits BCH corrected, summed.
    pub bch_corrected: u64,
    /// Es/N0 of the latest frame, from its known symbols.
    pub es_n0_db: Option<f32>,
    /// The latest valid BBHEADER.
    pub last_header: Option<BbHeader>,
    /// Good frames per input stream (ISI; 0 for a single stream).
    pub streams: BTreeMap<u8, u64>,
    /// (good, total) per MODCOD index.
    pub per_modcod: BTreeMap<u8, (u64, u64)>,
    /// Useful bit rate (BBHEADER DFL) over the last couple of seconds of
    /// signal, bits per second.
    pub payload_bps: f64,
    /// Fraction of real time the FEC thread is busy.
    pub load: f32,
}

/// Runs a [`FecDecoder`] on its own thread.
pub(crate) struct FecWorker {
    tx: Option<SyncSender<PlFrame>>,
    stats: Arc<Mutex<FecStats>>,
    join: Option<JoinHandle<()>>,
}

impl FecWorker {
    /// `symbol_rate` turns frame lengths into signal time for the rate.
    pub fn spawn(symbol_rate: f64) -> Self {
        let (tx, rx) = mpsc::sync_channel(QUEUE);
        let stats = Arc::new(Mutex::new(FecStats::default()));
        let shared = stats.clone();
        let join = std::thread::Builder::new()
            .name("decdvb-fec".into())
            .spawn(move || run(rx, shared, symbol_rate))
            .expect("spawn FEC thread");
        FecWorker {
            tx: Some(tx),
            stats,
            join: Some(join),
        }
    }

    /// Queue a frame; drop it (counted) if the thread is behind.
    pub fn offer(&self, f: PlFrame) {
        if let Some(tx) = &self.tx
            && let Err(TrySendError::Full(_)) = tx.try_send(f)
        {
            self.stats.lock().unwrap().dropped += 1;
        }
    }

    pub fn stats(&self) -> FecStats {
        self.stats.lock().unwrap().clone()
    }
}

impl Drop for FecWorker {
    fn drop(&mut self) {
        // Closing the queue ends the thread after its current frame.
        self.tx = None;
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

fn run(rx: Receiver<PlFrame>, stats: Arc<Mutex<FecStats>>, symbol_rate: f64) {
    let mut dec = FecDecoder::new();
    // Payload rate window: DFL bits and signal seconds.
    let (mut win_bits, mut win_secs) = (0f64, 0f64);
    let mut busy = 0f64;
    while let Ok(f) = rx.recv() {
        let t0 = Instant::now();
        let secs = f.pls.plframe_len as f64 / symbol_rate;
        let out = dec.decode(&f);
        let used = t0.elapsed().as_secs_f64();
        busy = 0.95 * busy + 0.05 * (used / secs.max(1e-9));

        let mut s = stats.lock().unwrap();
        s.load = busy as f32;
        win_secs += secs;
        if let Some(b) = out {
            s.frames += 1;
            s.iterations += b.ldpc.iterations as u64;
            s.ldpc_unconverged += !b.ldpc.converged as u64;
            s.es_n0_db = Some(b.es_n0_db);
            let entry = s.per_modcod.entry(b.pls.modcod).or_default();
            entry.1 += 1;
            match (&b.bch, &b.header) {
                (Err(_), _) => s.bch_failed += 1,
                (Ok(_), Err(_)) => s.crc_failed += 1,
                (Ok(c), Ok(h)) => {
                    s.ok += 1;
                    s.bch_corrected += *c as u64;
                    s.last_header = Some(*h);
                    *s.streams
                        .entry(if h.single_stream { 0 } else { h.isi })
                        .or_default() += 1;
                    s.per_modcod.entry(b.pls.modcod).or_default().0 += 1;
                    win_bits += h.dfl as f64;
                }
            }
        }
        if win_secs >= 2.0 {
            s.payload_bps = win_bits / win_secs;
            (win_bits, win_secs) = (0.0, 0.0);
        } else if s.payload_bps == 0.0 && win_secs > 0.2 {
            // A first figure quickly, refined once the window fills.
            s.payload_bps = win_bits / win_secs;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demod::Demod;
    use decdvb_frame::StreamFormat;
    use decdvb_mod::{FrameSpec, PlFramer, Shaper, TsBbFramer};
    use std::f64::consts::TAU;

    /// A real DVB-S2 signal: `schedule` frames, shaped at 4 samples per
    /// symbol, offset by `cycles` per symbol, at `esn0_db`.
    fn signal(
        schedule: &[FrameSpec],
        n_sym: usize,
        cycles: f64,
        esn0_db: f64,
        seed: u64,
    ) -> Vec<Iq> {
        let mut framer = PlFramer::new(0, seed);
        let syms = framer.build_schedule(schedule, n_sym);
        let mut sh = Shaper::new(4, 0.25, 16);
        let mut x = Vec::new();
        sh.process(&syms, &mut x);
        let mut s = seed | 1;
        let mut uniform = move || {
            s ^= s >> 12;
            s ^= s << 25;
            s ^= s >> 27;
            (s.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
        };
        // The shaper's output has unit power per *sample*, so a symbol's
        // energy after the matched filter is 4× that: N0 per sample is
        // 4 · 10^(−Es/N0 / 10). (The receiver's estimate caught this when the
        // test first had it per symbol: it read 8.6 dB for "2.5".)
        let sigma = (4.0 * 10f64.powf(-esn0_db / 10.0)).sqrt() * std::f64::consts::FRAC_1_SQRT_2;
        x.iter()
            .enumerate()
            .map(|(n, &v)| {
                let ph = TAU * cycles * n as f64 / 4.0 + 0.9;
                let r = (-2.0 * uniform().max(1e-300).ln()).sqrt() * sigma;
                let t = TAU * uniform();
                v * Iq::new(ph.cos() as f32, ph.sin() as f32)
                    + Iq::new((r * t.cos()) as f32, (r * t.sin()) as f32)
            })
            .collect()
    }

    /// The BBFRAMEs a `PlFramer` with `seed` sends for `schedule`, in order,
    /// descrambled.
    fn expected(schedule: &[FrameSpec], frames: usize, seed: u64) -> Vec<Vec<u8>> {
        let mut ts = TsBbFramer::new(seed ^ 0x7E57);
        ts.ccm = schedule.iter().filter(|s| s.modcod != 0).all(|s| {
            (s.modcod, s.short_fecframe) == (schedule[0].modcod, schedule[0].short_fecframe)
        });
        schedule
            .iter()
            .cycle()
            .take(frames)
            .filter(|s| s.modcod != 0)
            .map(|s| {
                let size = if s.short_fecframe {
                    FecFrame::Short
                } else {
                    FecFrame::Normal
                };
                let mc = s2_modcod(s.modcod, size).unwrap();
                let p = FecParams::new(size, mc.rate).unwrap();
                let mut f = ts.next_frame(p.bbframe_bytes());
                bb_scramble(&mut f);
                f
            })
            .collect()
    }

    fn decode_all(x: &[Iq]) -> Vec<BbFrame> {
        let mut d = Demod::new(4.0, 1.0, 0.25, 0);
        let mut fec = FecDecoder::new();
        let mut frames = Vec::new();
        let mut out = Vec::new();
        for chunk in x.chunks(50_000) {
            frames.clear();
            d.process(chunk, &mut frames);
            out.extend(frames.iter().filter_map(|f| fec.decode(f)));
        }
        out
    }

    #[test]
    fn decodes_an_acm_carrier_to_the_bbframes_that_were_sent() {
        // QPSK 1/2, 8PSK 3/5 (the odd interleaver), short 16APSK 2/3, a dummy
        // and short 32APSK 3/4, 0.2 % off frequency, at 18 dB.
        let schedule = [
            FrameSpec::new(4, false, true),
            FrameSpec::new(12, false, true),
            FrameSpec::new(18, true, false),
            FrameSpec::new(0, false, false),
            FrameSpec::new(24, true, true),
        ];
        let x = signal(&schedule, 400_000, 0.002, 18.0, 21);
        let got = decode_all(&x);
        let want = expected(&schedule, 60, 21);
        assert!(got.len() >= 6, "only {} frames", got.len());
        // The first decoded frame is some frame of the sequence; every one
        // after must follow it exactly.
        let first = want
            .iter()
            .position(|w| *w == got[0].bytes)
            .expect("first frame not in the sent sequence");
        for (k, b) in got.iter().enumerate() {
            assert!(b.ok(), "frame {k}: {:?} {:?} {:?}", b.ldpc, b.bch, b.header);
            assert_eq!(
                b.bytes,
                want[first + k],
                "frame {k} (MODCOD {})",
                b.pls.modcod
            );
            let h = b.header.unwrap();
            assert_eq!(h.format, StreamFormat::Transport);
            assert!(!h.ccm, "an ACM schedule must say so");
        }
    }

    #[test]
    fn decodes_qpsk_near_its_threshold() {
        // QPSK 1/2 needs ~1 dB Es/N0 in theory. At 2 dB every frame must
        // come through — carrier and timing recovery, coherent header reads,
        // LLRs, LDPC, BCH — and the known-symbol Es/N0 must read true.
        // (Before the coherent header reads, frames failed from 6 dB down:
        // a QPSK 1/2 header read differentially as 3/5.)
        let schedule = [FrameSpec::new(4, false, true)];
        let x = signal(&schedule, 230_000, 0.001, 2.0, 22);
        let got = decode_all(&x);
        assert!(got.len() >= 5, "only {} frames", got.len());
        for b in &got {
            assert!(
                b.ok(),
                "{:?} {:?} {:?} at {:.1} dB",
                b.ldpc,
                b.bch,
                b.header,
                b.es_n0_db
            );
            assert!(
                (b.es_n0_db - 2.0).abs() < 0.5,
                "Es/N0 estimate {:.2}",
                b.es_n0_db
            );
        }
    }

    #[test]
    fn decodes_without_pilots_at_moderate_snr() {
        // No pilots: only the headers, 32 490 symbols apart, anchor the
        // phase; the decision-directed loop must hold in between. At 5 dB it
        // does; at 4 dB about one frame in seven is lost to a cycle slip —
        // which is what DVB-S2's pilots are for.
        let schedule = [FrameSpec::new(4, false, false)];
        let x = signal(&schedule, 230_000, 0.001, 5.0, 23);
        let got = decode_all(&x);
        assert!(got.len() >= 5, "only {} frames", got.len());
        let ok = got.iter().filter(|b| b.ok()).count();
        assert_eq!(ok, got.len(), "{ok} of {} frames", got.len());
    }
}
