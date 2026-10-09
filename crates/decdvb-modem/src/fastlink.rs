//! Teledyne Paradise Q-Flex **FastLink**: Paradise's own low-latency LDPC
//! family, unpublished. What is here was measured on a live Q-Flex carrier
//! (FastLink, QPSK, rate 0.710, 95 017 sym/s, closed network plus ESC):
//!
//! - a frame of 11 538 symbols: an 18-symbol sync word, then 11 520 symbols
//!   = 23 040 coded bits = four 5760-bit codeword slots;
//! - each slot holds an LDPC codeword of 4096 data bits (rank of thousands
//!   of received slots, cleaned of the few with bit errors): 4 × 4096 data
//!   bits in 23 076 symbols' worth of bits is the "0.710" of the menu;
//! - the code's checks are mostly of weight 16, its bits interleaved, and a
//!   fixed scrambling offset lies on the code bits.
//!
//! Enough to recognise the framing (this module); not yet enough to read the
//! data, which needs the data positions, their order and the data
//! scrambler.

use decdvb_core::Iq;

/// The sync word as QPSK symbols, in one carrier orientation: (I, Q) signs,
/// `true` for negative.
const UW: [(bool, bool); 18] = {
    // Measured labels (I bit, Q bit): 0,1,2,1,1,0,1,1,2,2,2,3,3,3,0,3,0,2.
    const L: [u8; 18] = [0, 1, 2, 1, 1, 0, 1, 1, 2, 2, 2, 3, 3, 3, 0, 3, 0, 2];
    let mut out = [(false, false); 18];
    let mut i = 0;
    while i < 18 {
        out[i] = (L[i] >> 1 == 1, L[i] & 1 == 1);
        i += 1;
    }
    out
};
/// Symbols per frame (QPSK, rate 0.710).
pub const FRAME_SYMBOLS: usize = 11_538;
pub const UW_SYMBOLS: usize = 18;
/// Coded bits per codeword slot, and the data bits in each.
pub const SLOT_BITS: usize = 5760;
pub const DATA_BITS: usize = 4096;
/// UW symbols allowed wrong.
const UW_ERRORS: usize = 2;

/// The sync word under one of the eight QPSK orientations (`k` quarter
/// turns, mirrored first if `conj`), as unit-scale points.
fn uw_points(k: u8, conj: bool) -> [Iq; UW_SYMBOLS] {
    let mut out = [Iq::new(0.0, 0.0); UW_SYMBOLS];
    for (o, &(i, q)) in out.iter_mut().zip(&UW) {
        let mut z = Iq::new(if i { -1.0 } else { 1.0 }, if q { -1.0 } else { 1.0 });
        if conj {
            z = z.conj();
        }
        for _ in 0..k {
            z *= Iq::new(0.0, 1.0);
        }
        *o = z;
    }
    out
}

/// Where the sync word appears in carrier-locked QPSK `symbols`, under
/// whichever orientation fits best; at least `min_frames` running.
pub fn find_frames(symbols: &[Iq], min_frames: usize) -> Option<Vec<usize>> {
    let quad = |z: Iq| (z.re < 0.0, z.im < 0.0);
    let q: Vec<(bool, bool)> = symbols.iter().map(|&z| quad(z)).collect();
    if q.len() < FRAME_SYMBOLS * min_frames + UW_SYMBOLS {
        return None;
    }
    for conj in [false, true] {
        for k in 0..4u8 {
            let uw: Vec<(bool, bool)> = uw_points(k, conj).iter().map(|&z| quad(z)).collect();
            let at = |p: usize| {
                q[p..p + UW_SYMBOLS]
                    .iter()
                    .zip(&uw)
                    .filter(|(a, b)| a != b)
                    .count()
                    <= UW_ERRORS
            };
            // The first place where it recurs min_frames times running.
            let last_start = q.len() - (min_frames - 1) * FRAME_SYMBOLS - UW_SYMBOLS;
            if let Some(p0) = (0..last_start.min(FRAME_SYMBOLS))
                .find(|&p| (0..min_frames).all(|m| at(p + m * FRAME_SYMBOLS)))
            {
                let hits = (0..)
                    .map(|m| p0 + m * FRAME_SYMBOLS)
                    .take_while(|&p| p + UW_SYMBOLS <= q.len())
                    .filter(|&p| at(p))
                    .collect();
                return Some(hits);
            }
        }
    }
    None
}

/// Whether carrier-locked QPSK symbols carry Q-Flex FastLink framing.
pub fn detect(symbols: &[Iq]) -> bool {
    find_frames(symbols, 3).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(n: usize, k: u8, conj: bool, seed: u64) -> Vec<Iq> {
        let mut s = seed | 1;
        let mut out = vec![Iq::new(0.7, 0.7); 1234];
        let uw = uw_points(k, conj);
        for _ in 0..n {
            out.extend_from_slice(&uw);
            for _ in 0..FRAME_SYMBOLS - UW_SYMBOLS {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                out.push(Iq::new(
                    if s & 1 == 0 { 1.0 } else { -1.0 },
                    if s & 2 == 0 { 1.0 } else { -1.0 },
                ));
            }
        }
        out
    }

    #[test]
    fn finds_the_framing_in_any_orientation() {
        for (k, conj) in [(0, false), (1, false), (3, true), (2, true)] {
            let x = frames(5, k, conj, 3);
            let hits = find_frames(&x, 3).expect("not found");
            assert_eq!(hits[0], 1234);
            assert_eq!(hits.len(), 5);
        }
    }

    #[test]
    fn random_qpsk_is_not_fastlink() {
        let mut s = 7u64;
        let x: Vec<Iq> = (0..60_000)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                Iq::new(
                    if s & 1 == 0 { 1.0 } else { -1.0 },
                    if s & 2 == 0 { 1.0 } else { -1.0 },
                )
            })
            .collect();
        assert!(!detect(&x));
    }
}
