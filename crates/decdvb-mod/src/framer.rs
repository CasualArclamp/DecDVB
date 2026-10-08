//! PLFRAME construction (ETSI EN 302 307-1 §5.5).
//!
//! A PLFRAME is the 90-symbol PLHEADER (SOF + PLS code, pi/2-BPSK, never
//! scrambled) followed by the payload: `S` slots of 90 data symbols with a
//! 36-symbol pilot block after every 16 slots when pilots are on, the whole
//! payload — pilots included — then PL-scrambled.
//!
//! The data symbols are real: a TS-mode BBFRAME of test packets, BCH and LDPC
//! encoded, interleaved and mapped (`fec`), so a test signal decodes end to
//! end. The one exception is a code the standard does not define (short
//! FECFRAME 9/10): its PLHEADER can still be built, with random points after.

use decdvb_core::{FecFrame, Modulation};
use decdvb_core::{Iq, s2_modcod};
use decdvb_fec::demap::map_fecframe;
use decdvb_fec::{Constellation, FecParams};
use decdvb_frame::pi2bpsk::map_bpsk;

use crate::fec::{BbFrameSource, FecEncoder, TsBbFramer};
use decdvb_frame::{
    PILOT_BLK_LEN, PLHEADER_LEN, PlScrambler, PlsInfo, PlscEncoder, SLOT_LEN, SLOTS_PER_PILOT_BLK,
    SOF_BIG_ENDIAN, SOF_LEN,
};

/// What one frame should carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameSpec {
    /// MODCOD 0..=28 (0 = dummy frame).
    pub modcod: u8,
    pub short_fecframe: bool,
    pub pilots: bool,
}

impl FrameSpec {
    pub const fn new(modcod: u8, short_fecframe: bool, pilots: bool) -> Self {
        FrameSpec {
            modcod,
            short_fecframe,
            pilots,
        }
    }

    pub fn info(&self) -> PlsInfo {
        PlsInfo::from_fields(self.modcod, self.short_fecframe, self.pilots)
    }
}

/// The unmodulated symbol used for pilots and dummy-frame payload:
/// `(1 + j) / sqrt(2)` before scrambling (§5.5.3).
const PILOT: Iq = Iq::new(
    std::f32::consts::FRAC_1_SQRT_2,
    std::f32::consts::FRAC_1_SQRT_2,
);

/// Builds PLFRAMEs. Holds the scrambler and PLS encoder so they are computed
/// once, and its own small PRNG so output is reproducible from a seed.
pub struct PlFramer {
    scrambler: PlScrambler,
    plsc: PlscEncoder,
    rng: u64,
    /// Where the BBFRAMEs come from (TS packets unless told otherwise).
    source: Box<dyn BbFrameSource>,
    fec: FecEncoder,
    data: Vec<Iq>,
}

impl PlFramer {
    pub fn new(gold_code: u32, seed: u64) -> Self {
        PlFramer {
            scrambler: PlScrambler::new(gold_code),
            plsc: PlscEncoder::new(),
            rng: seed | 1,
            source: Box::new(TsBbFramer::new(seed ^ 0x7E57)),
            fec: FecEncoder::new(),
            data: Vec::new(),
        }
    }

    /// Take BBFRAMEs from `source` instead (e.g. IP over GSE).
    pub fn with_source(mut self, source: Box<dyn BbFrameSource>) -> Self {
        self.source = source;
        self
    }

    /// The roll-off the BBHEADERs announce; match the shaping filter's.
    pub fn set_roll_off(&mut self, roll_off: decdvb_core::RollOff) {
        self.source.set_roll_off(roll_off);
    }

    fn next_u64(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Append one complete PLFRAME to `out`.
    ///
    /// # Panics
    /// If the MODCOD is not one of the 28 S2 MODCODs or 0 (dummy).
    pub fn build(&mut self, spec: FrameSpec, out: &mut Vec<Iq>) {
        let info = spec.info();
        let start = out.len();

        // PLHEADER: SOF then PLS code, pi/2-BPSK, continuous symbol index.
        out.resize(start + PLHEADER_LEN, Iq::new(0.0, 0.0));
        map_bpsk(SOF_BIG_ENDIAN, &mut out[start..start + SOF_LEN], SOF_LEN);
        self.plsc
            .encode(info.plsc, &mut out[start + SOF_LEN..start + PLHEADER_LEN]);

        let payload_start = out.len();
        if info.dummy_frame {
            // A dummy frame's payload is 36 slots of the unmodulated symbol.
            out.extend(std::iter::repeat_n(PILOT, info.xfecframe_len as usize));
        } else {
            let size = if spec.short_fecframe {
                FecFrame::Short
            } else {
                FecFrame::Normal
            };
            let mc = s2_modcod(spec.modcod, size)
                .unwrap_or_else(|| panic!("MODCOD {} is not a DVB-S2 MODCOD", spec.modcod));
            let cst = Constellation::for_modcod(mc.modulation, mc.rate)
                .unwrap_or_else(|| panic!("no constellation for {mc}"));

            self.data.clear();
            match FecParams::new(size, mc.rate) {
                Some(p) => {
                    let bb = self.source.next_frame(p.bbframe_bytes());
                    let fec = self.fec.encode(p, &bb);
                    map_fecframe(&fec, &cst, mc.rate, &mut self.data);
                }
                None => {
                    let mask = (1usize << cst.bits()) - 1;
                    for _ in 0..info.xfecframe_len {
                        let bits = (self.next_u64() >> 32) as usize & mask;
                        self.data.push(cst.map(bits));
                    }
                }
            }
            debug_assert_eq!(self.data.len(), info.xfecframe_len as usize);

            for (slot, data) in self.data.as_chunks::<SLOT_LEN>().0.iter().enumerate() {
                out.extend_from_slice(data);
                // A pilot block after every 16th slot, unless it would be last.
                let done = slot + 1;
                if info.has_pilots
                    && done % SLOTS_PER_PILOT_BLK == 0
                    && done < info.n_slots as usize
                {
                    out.extend(std::iter::repeat_n(PILOT, PILOT_BLK_LEN));
                }
            }
        }

        debug_assert_eq!(out.len() - start, info.plframe_len as usize);
        self.scrambler.scramble(&mut out[payload_start..]);
    }

    /// Build frames following `schedule`, cycling it until at least `n_symbols`
    /// have been produced. This is how ACM test signals are made: a schedule of
    /// several MODCODs changes the coding frame by frame.
    pub fn build_schedule(&mut self, schedule: &[FrameSpec], n_symbols: usize) -> Vec<Iq> {
        assert!(
            !schedule.is_empty(),
            "need at least one frame in the schedule"
        );
        let mut out = Vec::with_capacity(n_symbols + 40_000);
        // One MODCOD throughout is CCM; anything else is ACM/VCM (MATYPE).
        self.source
            .set_ccm(schedule.iter().filter(|s| s.modcod != 0).all(|s| {
                (s.modcod, s.short_fecframe) == (schedule[0].modcod, schedule[0].short_fecframe)
            }));
        let mut k = 0;
        while out.len() < n_symbols {
            self.build(schedule[k % schedule.len()], &mut out);
            k += 1;
        }
        out
    }
}

/// The modulation each S2 MODCOD uses, for callers that only have the index.
pub fn modulation_of(modcod: u8) -> Option<Modulation> {
    s2_modcod(modcod, FecFrame::Normal).map(|m| m.modulation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use decdvb_frame::{PlHeaderCorrelator, PlscDecoder, PlscDemap};

    #[test]
    fn frame_lengths_match_the_pls_geometry() {
        let mut f = PlFramer::new(0, 1);
        for spec in [
            FrameSpec::new(0, false, false),
            FrameSpec::new(4, false, false),
            FrameSpec::new(4, false, true),
            FrameSpec::new(14, true, true),
            FrameSpec::new(20, false, true),
            FrameSpec::new(27, true, false),
        ] {
            let mut out = Vec::new();
            f.build(spec, &mut out);
            assert_eq!(out.len(), spec.info().plframe_len as usize, "{spec:?}");
        }
    }

    #[test]
    fn header_decodes_back_to_the_spec() {
        let mut f = PlFramer::new(0, 2);
        let mut dec = PlscDecoder::new();
        for modcod in [0u8, 1, 11, 12, 17, 18, 23, 24, 28] {
            for (short, pilots) in [(false, false), (true, true)] {
                let spec = FrameSpec::new(modcod, short, pilots);
                let mut out = Vec::new();
                f.build(spec, &mut out);
                let got = dec.decode(&out[SOF_LEN - 1..], PlscDemap::CoherentSoft);
                assert_eq!(got, spec.info(), "{spec:?}");
            }
        }
    }

    #[test]
    fn payload_lies_on_the_constellation_after_descrambling() {
        // 16APSK 3/4 with pilots: descramble the payload and check every data
        // symbol is a constellation point and every pilot is (1+j)/sqrt(2).
        let spec = FrameSpec::new(19, false, true);
        let mut f = PlFramer::new(5, 3);
        let mut out = Vec::new();
        f.build(spec, &mut out);

        let mut payload = out[PLHEADER_LEN..].to_vec();
        PlScrambler::new(5).descramble(&mut payload);

        let cst = Constellation::apsk16(2.85);
        let info = spec.info();
        let mut i = 0;
        for slot in 0..info.n_slots as usize {
            for _ in 0..SLOT_LEN {
                let p = cst.map(cst.nearest(payload[i]));
                assert!(
                    (payload[i] - p).norm() < 1e-5,
                    "data symbol {i} off-constellation"
                );
                i += 1;
            }
            if (slot + 1) % 16 == 0 && slot + 1 < info.n_slots as usize {
                for _ in 0..PILOT_BLK_LEN {
                    assert!((payload[i] - PILOT).norm() < 1e-6, "pilot {i} wrong");
                    i += 1;
                }
            }
        }
        assert_eq!(i, payload.len());
    }

    #[test]
    fn acm_schedule_is_found_frame_by_frame() {
        // An ACM sequence: QPSK 1/2, 8PSK 3/4, 16APSK 5/6, a dummy, back to
        // QPSK. The correlator must find every header at exactly the spacing
        // the previous header's PLS code predicts.
        let schedule = [
            FrameSpec::new(4, false, true),
            FrameSpec::new(14, false, true),
            FrameSpec::new(21, true, false),
            FrameSpec::new(0, false, false),
        ];
        let mut f = PlFramer::new(0, 9);
        let stream = f.build_schedule(&schedule, 120_000);

        let mut corr = PlHeaderCorrelator::new();
        let mut dec = PlscDecoder::new();
        let mut found = Vec::new();
        for (i, &x) in stream.iter().enumerate() {
            if corr.push(x).is_some_and(|m| m > 0.9) {
                // `i` is the header's last symbol, so the PLS code starts at
                // i - 63 and the last SOF symbol is at i - 64.
                let info = dec.decode(&stream[i - 64..=i], PlscDemap::CoherentSoft);
                found.push((i, info));
            }
        }

        assert!(found.len() >= 6, "found only {} headers", found.len());
        for (k, w) in found.windows(2).enumerate() {
            let (i0, info0) = w[0];
            let (i1, _) = w[1];
            assert_eq!(
                i1 - i0,
                info0.plframe_len as usize,
                "frame {k}: spacing does not match MODCOD {}",
                info0.modcod
            );
            assert_eq!(info0.modcod, schedule[k % schedule.len()].modcod);
        }
    }
}
