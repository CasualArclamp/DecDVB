//! The interleaved (64, 7, 32) Reed–Muller code that protects the PLS code.
//!
//! ETSI EN 302 307-1 §5.5.2.4 and Figure 13b. Construction ported from
//! `gr-dvbs2rx`'s `lib/reed_muller.cc` (GPL-3).
//!
//! The 7-bit PLS dataword is protected by a code with minimum distance 32, so
//! it survives a remarkably bad channel — which matters because the PLHEADER
//! must be readable before anything else about the frame is known.
//!
//! Construction: the top 6 bits select a codeword of the (32, 6, 16) Reed–Muller
//! code RM(1,5) as a mod-2 sum of generator rows. The 7th (least significant)
//! bit then controls interleaving into 64 bits: with b7 = 0 each bit is
//! duplicated (`y1 y1 y2 y2 …`), and with b7 = 1 the copy is inverted
//! (`y1 !y1 y2 !y2 …`).

use crate::defs::N_PLSC_CODEWORDS;

/// Generator matrix of the (32, 6, 16) Reed–Muller code (EN 302 307-1, Fig 13b).
const G: [u32; 6] = [
    0x5555_5555,
    0x3333_3333,
    0x0f0f_0f0f,
    0x00ff_00ff,
    0x0000_ffff,
    0xffff_ffff,
];

/// Interleave two 32-bit words into `a31 b31 a30 b30 … a0 b0`.
const fn bit_interleave(a: u32, b: u32) -> u64 {
    let mut res = 0u64;
    let mut i = 0;
    while i < 32 {
        res |= ((a as u64) & (1u64 << i)) << (i + 1);
        res |= ((b as u64) & (1u64 << i)) << i;
        i += 1;
    }
    res
}

/// Build the table of all 128 codewords. Index `i` is the dataword, so decoding
/// returns the table index directly.
const fn build_lut() -> [u64; N_PLSC_CODEWORDS] {
    let mut lut = [0u64; N_PLSC_CODEWORDS];
    let mut i = 0usize;
    while i < 64 {
        // The 6-bit dataword's MSB (the standard's b1) multiplies G[0].
        let mut code32 = 0u32;
        let mut row = 0usize;
        while row < 6 {
            if i & (0x20 >> row) != 0 {
                code32 ^= G[row];
            }
            row += 1;
        }
        lut[2 * i] = bit_interleave(code32, code32); // b7 = 0
        lut[2 * i + 1] = bit_interleave(code32, !code32); // b7 = 1
        i += 1;
    }
    lut
}

/// All 128 codewords, built at compile time.
pub static CODEWORDS: [u64; N_PLSC_CODEWORDS] = build_lut();

/// Encoder/decoder for the PLS Reed–Muller code.
///
/// Holds the Euclidean-space (2-PAM) images of every codeword so that soft
/// decoding is a plain maximum-inner-product search. The images are built from
/// the **scrambled** codewords, which folds descrambling into the table and
/// saves a step per frame.
pub struct ReedMuller {
    /// 2-PAM images, 64 floats per codeword, laid out contiguously.
    images: Vec<f32>,
    /// Scrambled codewords, for hard (Hamming-distance) decoding.
    scrambled: [u64; N_PLSC_CODEWORDS],
    /// Which datawords may appear. Narrowing this when the MODCOD set is known
    /// in advance both speeds decoding up and makes it more reliable.
    enabled: Vec<u8>,
}

impl Default for ReedMuller {
    fn default() -> Self {
        Self::new(0)
    }
}

impl ReedMuller {
    /// Build for all 128 datawords. `scrambler` is XORed into every codeword;
    /// pass [`crate::defs::PLSC_SCRAMBLER`] for the PLS code, or 0 for the bare
    /// Reed–Muller code.
    pub fn new(scrambler: u64) -> Self {
        Self::with_enabled(scrambler, (0..N_PLSC_CODEWORDS as u8).collect())
    }

    /// Build for a known subset of datawords.
    ///
    /// # Panics
    /// If `enabled` is empty or holds an index outside 0..128.
    pub fn with_enabled(scrambler: u64, enabled: Vec<u8>) -> Self {
        assert!(!enabled.is_empty(), "at least one dataword must be enabled");
        assert!(
            enabled.iter().all(|&i| (i as usize) < N_PLSC_CODEWORDS),
            "dataword indexes must be below 128"
        );

        let mut scrambled = [0u64; N_PLSC_CODEWORDS];
        let mut images = vec![0.0f32; N_PLSC_CODEWORDS * 64];
        for i in 0..N_PLSC_CODEWORDS {
            let cw = CODEWORDS[i] ^ scrambler;
            scrambled[i] = cw;
            for j in 0..64 {
                let bit = (cw >> (63 - j)) & 1;
                images[i * 64 + j] = 1.0 - 2.0 * bit as f32;
            }
        }
        ReedMuller {
            images,
            scrambled,
            enabled,
        }
    }

    /// The scrambled 64-bit codeword for a 7-bit dataword.
    ///
    /// # Panics
    /// If `dataword >= 128`.
    pub fn encode(&self, dataword: u8) -> u64 {
        assert!(
            (dataword as usize) < N_PLSC_CODEWORDS,
            "dataword must be 7 bits"
        );
        self.scrambled[dataword as usize]
    }

    /// Maximum-likelihood hard decode: the dataword whose scrambled codeword is
    /// closest in Hamming distance.
    pub fn decode_hard(&self, received: u64) -> u8 {
        let mut best = self.enabled[0];
        let mut best_dist = u32::MAX;
        for &i in &self.enabled {
            let dist = (received ^ self.scrambled[i as usize]).count_ones();
            if dist < best_dist {
                best_dist = dist;
                best = i;
            }
        }
        best
    }

    /// Soft decode from 64 de-rotated 2-PAM soft decisions (see
    /// [`crate::pi2bpsk::derotate_bpsk`]): the dataword maximising the inner
    /// product with the codeword's image.
    ///
    /// All images have equal norm, so maximising `<r, s(x)>` is the same as
    /// minimising `||r - s(x)||`, and no magnitude normalisation is needed.
    ///
    /// # Panics
    /// If `soft` is shorter than 64.
    pub fn decode_soft(&self, soft: &[f32]) -> u8 {
        assert!(soft.len() >= 64, "need 64 soft decisions");
        let mut best = self.enabled[0];
        let mut best_score = f32::NEG_INFINITY;
        for &i in &self.enabled {
            let image = &self.images[i as usize * 64..i as usize * 64 + 64];
            let score: f32 = soft[..64].iter().zip(image).map(|(r, s)| r * s).sum();
            if score > best_score {
                best_score = score;
                best = i;
            }
        }
        best
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::defs::PLSC_SCRAMBLER;
    use crate::pi2bpsk::{demap_bpsk, derotate_bpsk, map_bpsk};
    use decdvb_core::Iq;

    #[test]
    fn minimum_distance_is_32() {
        // The defining property of the (64, 7, 32) code: every pair of
        // codewords differs in at least 32 of the 64 bits.
        let mut min = u32::MAX;
        for (a, &wa) in CODEWORDS.iter().enumerate() {
            for &wb in &CODEWORDS[a + 1..] {
                min = min.min((wa ^ wb).count_ones());
            }
        }
        assert_eq!(min, 32);
    }

    #[test]
    fn codewords_are_distinct() {
        let mut seen = CODEWORDS;
        seen.sort_unstable();
        for w in seen.windows(2) {
            assert_ne!(w[0], w[1], "duplicate codeword {:#018x}", w[0]);
        }
    }

    #[test]
    fn b7_controls_the_interleave() {
        // With b7 = 0 the 32 bit-pairs are equal; with b7 = 1 they differ.
        for i in 0..64usize {
            let even = CODEWORDS[2 * i];
            let odd = CODEWORDS[2 * i + 1];
            for pair in 0..32 {
                let shift = 62 - 2 * pair;
                let a = (even >> (shift + 1)) & 1;
                let b = (even >> shift) & 1;
                assert_eq!(a, b, "b7=0 pair {pair} of dataword {}", 2 * i);

                let a = (odd >> (shift + 1)) & 1;
                let b = (odd >> shift) & 1;
                assert_ne!(a, b, "b7=1 pair {pair} of dataword {}", 2 * i + 1);
            }
        }
    }

    #[test]
    fn hard_decode_round_trips_every_dataword() {
        let rm = ReedMuller::new(PLSC_SCRAMBLER);
        for d in 0..N_PLSC_CODEWORDS as u8 {
            assert_eq!(rm.decode_hard(rm.encode(d)), d, "dataword {d}");
        }
    }

    #[test]
    fn hard_decode_corrects_up_to_15_bit_errors() {
        // Minimum distance 32 guarantees correction of floor((32-1)/2) = 15.
        let rm = ReedMuller::new(PLSC_SCRAMBLER);
        for d in [0u8, 1, 42, 113, 127] {
            let cw = rm.encode(d);
            for nerr in 0..=15u32 {
                // Flip the lowest `nerr` bits — an arbitrary but deterministic
                // error pattern of the right weight.
                let mut corrupted = cw;
                for b in 0..nerr {
                    corrupted ^= 1u64 << b;
                }
                assert_eq!(
                    rm.decode_hard(corrupted),
                    d,
                    "dataword {d} with {nerr} errors"
                );
            }
        }
    }

    #[test]
    fn soft_decode_round_trips_through_pi2_bpsk() {
        // The full PLS path: encode, map to pi/2-BPSK, de-rotate, soft decode.
        let rm = ReedMuller::new(PLSC_SCRAMBLER);
        let mut sym = [Iq::new(0.0, 0.0); 64];
        let mut soft = [0.0f32; 64];
        for d in 0..N_PLSC_CODEWORDS as u8 {
            map_bpsk(rm.encode(d), &mut sym, 64);
            derotate_bpsk(&sym, &mut soft, 64);
            assert_eq!(rm.decode_soft(&soft), d, "dataword {d}");
        }
    }

    #[test]
    fn hard_decode_round_trips_through_pi2_bpsk() {
        let rm = ReedMuller::new(PLSC_SCRAMBLER);
        let mut sym = [Iq::new(0.0, 0.0); 64];
        for d in 0..N_PLSC_CODEWORDS as u8 {
            map_bpsk(rm.encode(d), &mut sym, 64);
            assert_eq!(rm.decode_hard(demap_bpsk(&sym, 64)), d, "dataword {d}");
        }
    }

    #[test]
    fn narrowing_the_enabled_set_still_decodes_those() {
        let enabled = vec![4u8, 20, 68, 100];
        let rm = ReedMuller::with_enabled(PLSC_SCRAMBLER, enabled.clone());
        for d in enabled {
            assert_eq!(rm.decode_hard(rm.encode(d)), d);
        }
    }

    #[test]
    fn unscrambled_code_is_linear() {
        // Without the scrambler the code is linear, so the all-zero dataword
        // maps to the all-zero codeword.
        assert_eq!(CODEWORDS[0], 0);
        let rm = ReedMuller::new(0);
        assert_eq!(rm.encode(0), 0);
    }
}
