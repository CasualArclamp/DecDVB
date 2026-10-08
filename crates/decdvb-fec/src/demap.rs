//! Bit interleaving and (de)mapping between FECFRAME bits and symbols
//! (EN 302 307-1 §5.3.3 and §5.4).
//!
//! **Interleaver.** QPSK has none: symbol s carries bits 2s, 2s+1. For 8PSK,
//! 16APSK and 32APSK the FECFRAME is written column-wise into an m-column
//! block (rows = N/m, Table 8) and read row-wise: symbol s is row s, its label
//! MSB from column 0 — so the first FECFRAME bit is symbol 0's MSB. The one
//! exception in EN 302 307-1 is 8PSK rate 3/5, read columns 2, 1, 0: the
//! first FECFRAME bit becomes symbol 0's *third* bit (Figure 7). Cross-checked
//! against gr-dvbs2rx's `xfecframe_demapper_cb_impl.cc` (GPL-3).
//!
//! **Soft demapping** is max-log: for each label bit, the squared distance to
//! the nearest point with that bit 1 minus the nearest with it 0, over the
//! noise variance. Positive means bit 0, the LDPC decoder's convention.

use decdvb_core::{CodeRate, Iq, Modulation};

use crate::constellation::Constellation;

/// Which interleaver column feeds each label bit, MSB first; `None` for no
/// interleaving (QPSK).
fn columns(modulation: Modulation, rate: CodeRate) -> Option<&'static [usize]> {
    match modulation {
        Modulation::Psk8 if (rate.num, rate.den) == (3, 5) => Some(&[2, 1, 0]),
        Modulation::Psk8 => Some(&[0, 1, 2]),
        Modulation::Apsk16 => Some(&[0, 1, 2, 3]),
        Modulation::Apsk32 => Some(&[0, 1, 2, 3, 4]),
        _ => None,
    }
}

/// FECFRAME bit index of label bit `b` of symbol `s`.
#[inline]
fn bit_index(cols: Option<&[usize]>, m: usize, rows: usize, s: usize, b: usize) -> usize {
    match cols {
        Some(c) => c[b] * rows + s,
        None => s * m + b,
    }
}

/// Map a FECFRAME (N bits as bytes, MSB first) to symbols: interleave, then
/// look each label up in `cst`. Appends N/m symbols to `out`.
pub fn map_fecframe(fecframe: &[u8], cst: &Constellation, rate: CodeRate, out: &mut Vec<Iq>) {
    let n = fecframe.len() * 8;
    let m = cst.bits() as usize;
    assert_eq!(n % m, 0);
    let rows = n / m;
    let cols = columns(cst.modulation, rate);
    let bit = |i: usize| ((fecframe[i / 8] >> (7 - i % 8)) & 1) as usize;
    out.extend((0..rows).map(|s| {
        let label = (0..m).fold(0, |acc, b| (acc << 1) | bit(bit_index(cols, m, rows, s, b)));
        cst.map(label)
    }));
}

/// Max-log LLRs for every FECFRAME bit, de-interleaved into FECFRAME order.
///
/// `symbols` are the frame's data symbols (pilots removed), scaled so `cst`'s
/// points are where they should land; `noise_var` is the noise power per
/// complex symbol on that scale. `out` receives N = symbols × m values.
pub fn demap_llr(
    symbols: &[Iq],
    cst: &Constellation,
    rate: CodeRate,
    noise_var: f32,
    out: &mut [f32],
) {
    let m = cst.bits() as usize;
    let rows = symbols.len();
    assert_eq!(out.len(), rows * m);
    let cols = columns(cst.modulation, rate);
    let inv = 1.0 / noise_var.max(1e-6);
    let npts = cst.points.len();
    let mut d = [0f32; 32];
    for (s, &y) in symbols.iter().enumerate() {
        for (i, p) in cst.points.iter().enumerate() {
            d[i] = (y - p).norm_sqr();
        }
        for b in 0..m {
            let shift = m - 1 - b;
            let (mut d0, mut d1) = (f32::INFINITY, f32::INFINITY);
            for (i, &di) in d[..npts].iter().enumerate() {
                if (i >> shift) & 1 == 0 {
                    d0 = d0.min(di);
                } else {
                    d1 = d1.min(di);
                }
            }
            out[bit_index(cols, m, rows, s, b)] = (d1 - d0) * inv;
        }
    }
}

/// Quantize LLRs for the LDPC decoder: `scale` steps per LLR unit, clamped to
/// the symmetric 8-bit range. The decoder is indifferent to the scale between
/// about 2 and 8 (see its tests); 4 keeps strong bits clear of the clamp.
pub fn quantize(llr: &[f32], scale: f32, out: &mut [i8]) {
    for (o, &l) in out.iter_mut().zip(llr) {
        *o = (l * scale).round().clamp(-127.0, 127.0) as i8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_s2_mappings() -> Vec<(Constellation, CodeRate)> {
        let mut v = Vec::new();
        for (m, rates) in [
            (Modulation::Qpsk, vec![(1, 2), (9, 10)]),
            (Modulation::Psk8, vec![(3, 5), (2, 3), (9, 10)]),
            (Modulation::Apsk16, vec![(2, 3), (9, 10)]),
            (Modulation::Apsk32, vec![(3, 4), (9, 10)]),
        ] {
            for (n, d) in rates {
                let r = CodeRate::new(n, d);
                v.push((Constellation::for_modcod(m, r).unwrap(), r));
            }
        }
        v
    }

    #[test]
    fn noiseless_round_trip_for_every_constellation() {
        let mut s = 0x1234_5678_9abc_def1u64;
        for (cst, rate) in all_s2_mappings() {
            // 1800 bytes: divisible by 2, 3, 4 and 5 bits per symbol.
            let frame: Vec<u8> = (0..1800)
                .map(|_| {
                    s ^= s >> 12;
                    s ^= s << 25;
                    s ^= s >> 27;
                    (s.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 56) as u8
                })
                .collect();
            let mut sym = Vec::new();
            map_fecframe(&frame, &cst, rate, &mut sym);
            let mut llr = vec![0f32; frame.len() * 8];
            demap_llr(&sym, &cst, rate, 0.01, &mut llr);
            for (i, &l) in llr.iter().enumerate() {
                let bit = (frame[i / 8] >> (7 - i % 8)) & 1;
                assert_eq!(l < 0.0, bit == 1, "{:?} {rate}: bit {i}", cst.modulation);
                assert!(l.abs() > 1.0, "{:?} {rate}: weak bit {i}", cst.modulation);
            }
        }
    }

    #[test]
    fn interleaver_puts_the_first_bit_where_table_8_says() {
        // A frame whose only 1 is its first bit.
        let mut frame = vec![0u8; 1800];
        frame[0] = 0x80;
        let rows = |m: usize| 1800 * 8 / m;
        let check = |m: Modulation, r: (u16, u16), label: usize| {
            let rate = CodeRate::new(r.0, r.1);
            let cst = Constellation::for_modcod(m, rate).unwrap();
            let mut sym = Vec::new();
            map_fecframe(&frame, &cst, rate, &mut sym);
            assert_eq!(sym.len(), rows(cst.bits() as usize));
            // Symbol 0 carries it, at the expected label bit; the rest are 0.
            assert_eq!(cst.nearest(sym[0]), label, "{m:?} {rate}");
            assert!(
                sym[1..].iter().all(|&y| cst.nearest(y) == 0),
                "{m:?} {rate}"
            );
        };
        check(Modulation::Qpsk, (1, 2), 0b10);
        check(Modulation::Psk8, (2, 3), 0b100);
        check(Modulation::Psk8, (3, 5), 0b001); // read out third
        check(Modulation::Apsk16, (2, 3), 0b1000);
        check(Modulation::Apsk32, (3, 4), 0b10000);
    }

    #[test]
    fn llrs_scale_with_the_noise() {
        let cst = Constellation::qpsk();
        let rate = CodeRate::new(1, 2);
        let y = [Iq::new(0.3, -0.2)];
        let mut a = [0f32; 2];
        let mut b = [0f32; 2];
        demap_llr(&y, &cst, rate, 0.5, &mut a);
        demap_llr(&y, &cst, rate, 0.25, &mut b);
        // QPSK max-log is exact: 2·√2·x / σ² per axis (points at ±1/√2).
        let k = 2.0 * std::f32::consts::SQRT_2 / 0.5;
        assert!((a[0] - k * 0.3).abs() < 1e-5 && (a[1] - k * -0.2).abs() < 1e-5);
        assert!((b[0] - 2.0 * a[0]).abs() < 1e-5);
    }
}
