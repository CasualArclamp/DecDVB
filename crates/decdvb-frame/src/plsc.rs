//! The physical-layer signalling code: 7 bits that describe the frame.
//!
//! ETSI EN 302 307-1 §5.5.2. The PLS code is the whole basis of ACM: every
//! PLFRAME announces its own MODCOD, FECFRAME length and whether it carries
//! pilots, so a receiver can follow a transmitter that changes coding from one
//! frame to the next without being told anything in advance.
//!
//! Layout of the 7-bit dataword: `MODCOD (5 bits) | short FECFRAME | pilots`.
//!
//! Frame-geometry derivation cross-checked against `gr-dvbs2rx`'s
//! `lib/pl_signaling.cc` (GPL-3).

use decdvb_core::Iq;

use crate::defs::{PILOT_BLK_LEN, PLSC_LEN, PLSC_SCRAMBLER, SLOT_LEN, SLOTS_PER_PILOT_BLK};
use crate::pi2bpsk::{demap_bpsk, demap_bpsk_diff, derotate_bpsk, map_bpsk};
use crate::rm::ReedMuller;

/// Everything the PLS code tells us about a PLFRAME.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlsInfo {
    /// The raw 7-bit PLS code.
    pub plsc: u8,
    /// MODCOD index, 0..=31. 0 means a dummy frame.
    pub modcod: u8,
    /// Short FECFRAME (16 200 bits) rather than normal (64 800).
    pub short_fecframe: bool,
    /// The PLFRAME carries pilot blocks.
    pub has_pilots: bool,
    /// A dummy frame: no payload, sent to fill time when there is no data.
    pub dummy_frame: bool,
    /// Bits per constellation symbol (0 for a dummy frame).
    pub n_mod: u8,
    /// Payload slots of 90 symbols.
    pub n_slots: u16,
    /// Number of pilot blocks.
    pub n_pilots: u8,
    /// Whole PLFRAME length in symbols: PLHEADER + payload + pilots.
    pub plframe_len: u32,
    /// Payload length in symbols: data plus pilots, excluding the PLHEADER.
    pub payload_len: u32,
    /// XFECFRAME length in symbols: data only, no pilots.
    pub xfecframe_len: u32,
}

impl PlsInfo {
    /// Derive the frame geometry from a 7-bit PLS code.
    ///
    /// Only the low 7 bits are used.
    pub fn parse(plsc: u8) -> Self {
        let plsc = plsc & 0x7F;
        let modcod = plsc >> 2;
        let short_fecframe = plsc & 0x2 != 0;
        let dummy_frame = modcod == 0;
        // A dummy frame never carries pilots, whatever the bit says.
        let has_pilots = (plsc & 0x1 != 0) && !dummy_frame;

        // Slots per frame follow from the bits per symbol: a normal FECFRAME is
        // 64 800 bits, and a slot is 90 symbols, so S = 64800 / (90 * n_mod).
        let (n_mod, mut n_slots): (u8, u16) = match modcod {
            1..=11 => (2, 360),  // QPSK
            12..=17 => (3, 240), // 8PSK
            18..=23 => (4, 180), // 16APSK
            24..=28 => (5, 144), // 32APSK
            _ => (0, 36),        // dummy frame (and the reserved indexes)
        };

        // A short FECFRAME is a quarter the length, so a quarter the slots.
        if short_fecframe && !dummy_frame {
            n_slots >>= 2;
        }

        // One pilot block after every 16 slots, but not a trailing one.
        let n_pilots = if has_pilots {
            ((n_slots as usize - 1) / SLOTS_PER_PILOT_BLK) as u8
        } else {
            0
        };

        let xfecframe_len = n_slots as u32 * SLOT_LEN as u32;
        // The PLHEADER is itself exactly one slot long.
        let plframe_len =
            (n_slots as u32 + 1) * SLOT_LEN as u32 + n_pilots as u32 * PILOT_BLK_LEN as u32;

        PlsInfo {
            plsc,
            modcod,
            short_fecframe,
            has_pilots,
            dummy_frame,
            n_mod,
            n_slots,
            n_pilots,
            plframe_len,
            payload_len: plframe_len - SLOT_LEN as u32,
            xfecframe_len,
        }
    }

    /// Build from the fields instead of a raw code.
    pub fn from_fields(modcod: u8, short_fecframe: bool, has_pilots: bool) -> Self {
        Self::parse(((modcod & 0x1F) << 2) | ((short_fecframe as u8) << 1) | has_pilots as u8)
    }
}

/// Encodes a PLS code into its 64 pi/2-BPSK symbols.
pub struct PlscEncoder {
    rm: ReedMuller,
}

impl Default for PlscEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl PlscEncoder {
    pub fn new() -> Self {
        PlscEncoder {
            rm: ReedMuller::new(PLSC_SCRAMBLER),
        }
    }

    /// Write the 64 PLS symbols for a raw 7-bit code.
    ///
    /// # Panics
    /// If `out` is shorter than 64.
    pub fn encode(&self, plsc: u8, out: &mut [Iq]) {
        map_bpsk(self.rm.encode(plsc & 0x7F), out, PLSC_LEN);
    }

    /// Write the 64 PLS symbols for the given fields.
    pub fn encode_fields(
        &self,
        modcod: u8,
        short_fecframe: bool,
        has_pilots: bool,
        out: &mut [Iq],
    ) {
        let plsc = ((modcod & 0x1F) << 2) | ((short_fecframe as u8) << 1) | has_pilots as u8;
        self.encode(plsc, out);
    }
}

/// How to demap the PLS symbols.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlscDemap {
    /// Coherent soft decisions — the best option once the carrier is locked,
    /// worth roughly 2 dB over hard decisions.
    CoherentSoft,
    /// Coherent hard decisions.
    CoherentHard,
    /// Differential hard decisions — works with an unknown or drifting carrier
    /// phase, at a cost in sensitivity. Used during acquisition.
    Differential,
}

/// Decodes 64 noisy pi/2-BPSK symbols back into a PLS code.
pub struct PlscDecoder {
    rm: ReedMuller,
    soft: Vec<f32>,
}

impl Default for PlscDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl PlscDecoder {
    pub fn new() -> Self {
        PlscDecoder {
            rm: ReedMuller::new(PLSC_SCRAMBLER),
            soft: vec![0.0; PLSC_LEN],
        }
    }

    /// Restrict decoding to a known set of PLS codes.
    ///
    /// Worth doing on a carrier whose MODCOD set is known: it cannot then
    /// mis-decode into a MODCOD that cannot occur.
    pub fn with_expected(expected: Vec<u8>) -> Self {
        PlscDecoder {
            rm: ReedMuller::with_enabled(PLSC_SCRAMBLER, expected),
            soft: vec![0.0; PLSC_LEN],
        }
    }

    /// Decode a PLHEADER.
    ///
    /// `symbols` must start at **the last SOF symbol**, followed by the 64 PLS
    /// symbols, so `symbols.len() >= 65`. The coherent modes skip that first
    /// symbol; the differential mode needs it to seed the chain. Requiring it
    /// in every mode keeps the caller's slicing identical whichever is used.
    ///
    /// # Panics
    /// If `symbols` holds fewer than 65 symbols.
    pub fn decode(&mut self, symbols: &[Iq], how: PlscDemap) -> PlsInfo {
        assert!(
            symbols.len() > PLSC_LEN,
            "need the last SOF symbol plus 64 PLS symbols"
        );
        let plsc = match how {
            PlscDemap::CoherentSoft => {
                derotate_bpsk(&symbols[1..], &mut self.soft, PLSC_LEN);
                self.rm.decode_soft(&self.soft)
            }
            PlscDemap::CoherentHard => self.rm.decode_hard(demap_bpsk(&symbols[1..], PLSC_LEN)),
            PlscDemap::Differential => self.rm.decode_hard(demap_bpsk_diff(symbols, PLSC_LEN)),
        };
        PlsInfo::parse(plsc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::defs::{MAX_PLFRAME_LEN, PLHEADER_LEN, SOF_BIG_ENDIAN, SOF_LEN};

    /// Build a full 90-symbol PLHEADER for the given fields.
    fn plheader(modcod: u8, short_fecframe: bool, has_pilots: bool) -> Vec<Iq> {
        let mut out = vec![Iq::new(0.0, 0.0); PLHEADER_LEN];
        map_bpsk(SOF_BIG_ENDIAN, &mut out[..SOF_LEN], SOF_LEN);
        PlscEncoder::new().encode_fields(modcod, short_fecframe, has_pilots, &mut out[SOF_LEN..]);
        out
    }

    #[test]
    fn qpsk_normal_frame_geometry() {
        // MODCOD 4 = QPSK 1/2, normal FECFRAME, no pilots.
        let p = PlsInfo::from_fields(4, false, false);
        assert_eq!(p.modcod, 4);
        assert!(!p.short_fecframe && !p.has_pilots && !p.dummy_frame);
        assert_eq!(p.n_mod, 2);
        assert_eq!(p.n_slots, 360);
        assert_eq!(p.n_pilots, 0);
        assert_eq!(p.xfecframe_len, 32_400); // 64800 bits / 2 bits per symbol
        assert_eq!(p.plframe_len, 32_400 + 90);
        assert_eq!(p.payload_len, 32_400);
    }

    #[test]
    fn slots_match_the_fecframe_length_for_every_modcod() {
        // S * 90 * n_mod must equal the FECFRAME length, which is the identity
        // the slot counts come from.
        for modcod in 1..=28u8 {
            for short in [false, true] {
                let p = PlsInfo::from_fields(modcod, short, false);
                let bits = p.xfecframe_len * p.n_mod as u32;
                let expected = if short { 16_200 } else { 64_800 };
                assert_eq!(bits, expected, "MODCOD {modcod}, short={short}");
            }
        }
    }

    #[test]
    fn pilot_blocks_follow_the_16_slot_rule() {
        // Normal QPSK: 360 slots -> (360-1)/16 = 22 pilot blocks.
        let p = PlsInfo::from_fields(4, false, true);
        assert_eq!(p.n_pilots, 22);
        assert_eq!(p.plframe_len, 90 + 360 * 90 + 22 * 36);
        assert_eq!(p.plframe_len as usize, MAX_PLFRAME_LEN);

        // Short 32APSK: 144/4 = 36 slots -> (36-1)/16 = 2 pilot blocks.
        let p = PlsInfo::from_fields(24, true, true);
        assert_eq!(p.n_slots, 36);
        assert_eq!(p.n_pilots, 2);
    }

    #[test]
    fn longest_frame_is_normal_qpsk_with_pilots() {
        // Nothing may exceed the buffer size the acquisition code allocates.
        let mut worst = 0u32;
        for modcod in 0..32u8 {
            for short in [false, true] {
                for pilots in [false, true] {
                    worst = worst.max(PlsInfo::from_fields(modcod, short, pilots).plframe_len);
                }
            }
        }
        assert_eq!(worst as usize, MAX_PLFRAME_LEN);
    }

    #[test]
    fn dummy_frame_has_no_payload_and_no_pilots() {
        // MODCOD 0 is a dummy frame; the pilots bit must be ignored.
        let p = PlsInfo::from_fields(0, false, true);
        assert!(p.dummy_frame);
        assert!(!p.has_pilots);
        assert_eq!(p.n_pilots, 0);
        assert_eq!(p.n_mod, 0);
        assert_eq!(p.n_slots, 36);
        assert_eq!(p.plframe_len, 37 * 90);
    }

    #[test]
    fn modcod_ranges_give_the_right_constellation() {
        let bits = |mc: u8| PlsInfo::from_fields(mc, false, false).n_mod;
        assert_eq!(bits(1), 2); // QPSK 1/4
        assert_eq!(bits(11), 2); // QPSK 9/10
        assert_eq!(bits(12), 3); // 8PSK 3/5
        assert_eq!(bits(17), 3); // 8PSK 9/10
        assert_eq!(bits(18), 4); // 16APSK 2/3
        assert_eq!(bits(23), 4); // 16APSK 9/10
        assert_eq!(bits(24), 5); // 32APSK 3/4
        assert_eq!(bits(28), 5); // 32APSK 9/10
    }

    #[test]
    fn round_trip_every_plsc_in_all_three_demap_modes() {
        let mut dec = PlscDecoder::new();
        for modcod in 0..32u8 {
            for short in [false, true] {
                for pilots in [false, true] {
                    let header = plheader(modcod, short, pilots);
                    // The decoder wants the last SOF symbol first.
                    let from_last_sof = &header[SOF_LEN - 1..];
                    let want = PlsInfo::from_fields(modcod, short, pilots);

                    for how in [
                        PlscDemap::CoherentSoft,
                        PlscDemap::CoherentHard,
                        PlscDemap::Differential,
                    ] {
                        let got = dec.decode(from_last_sof, how);
                        assert_eq!(
                            got, want,
                            "MODCOD {modcod}, short={short}, pilots={pilots}, {how:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn differential_decode_survives_a_phase_offset() {
        let mut dec = PlscDecoder::new();
        let header = plheader(14, false, true); // 8PSK 3/4 with pilots
        let want = PlsInfo::from_fields(14, false, true);

        for turns in [0.13f32, 0.25, 0.5, 0.77] {
            let th = std::f32::consts::TAU * turns;
            let rot = Iq::new(th.cos(), th.sin());
            let rotated: Vec<Iq> = header[SOF_LEN - 1..].iter().map(|s| s * rot).collect();
            assert_eq!(
                dec.decode(&rotated, PlscDemap::Differential),
                want,
                "{turns} turns"
            );
        }
    }

    #[test]
    fn soft_decode_survives_heavy_noise() {
        // Minimum distance 32 should shrug off noise that visibly wrecks the
        // individual symbols. Deterministic pseudo-noise keeps the test stable.
        let mut dec = PlscDecoder::new();
        let header = plheader(4, false, false);
        let want = PlsInfo::from_fields(4, false, false);

        let mut state = 0x1234_5678_9ABC_DEF0u64;
        let mut next = || {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            let x = state.wrapping_mul(0x2545_F491_4F6C_DD1D);
            // Roughly uniform in [-1, 1).
            ((x >> 11) as f32 / (1u64 << 52) as f32) - 1.0
        };

        let noisy: Vec<Iq> = header[SOF_LEN - 1..]
            .iter()
            .map(|s| s + Iq::new(next() * 0.8, next() * 0.8))
            .collect();
        assert_eq!(dec.decode(&noisy, PlscDemap::CoherentSoft), want);
    }

    #[test]
    fn restricting_expected_codes_avoids_impossible_modcods() {
        // A receiver told to expect only QPSK 1/2 and 3/4 (no pilots) must
        // never report anything else, even from pure noise.
        let a = PlsInfo::from_fields(4, false, false).plsc;
        let b = PlsInfo::from_fields(7, false, false).plsc;
        let mut dec = PlscDecoder::with_expected(vec![a, b]);
        let noise = vec![Iq::new(0.01, -0.02); PLSC_LEN + 1];
        let got = dec.decode(&noise, PlscDemap::CoherentSoft);
        assert!(got.plsc == a || got.plsc == b, "got {:#x}", got.plsc);
    }
}
