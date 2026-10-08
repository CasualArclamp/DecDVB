//! DVB-S2 constellations (ETSI EN 302 307-1 §5.4).
//!
//! Each constellation is a table indexed by the symbol's bits, MSB first, so
//! `points[0b101]` is the point the bits `1 0 1` map to. All are normalised to
//! unit average power, which is what the PL framing and the demapper assume.
//!
//! The APSK ring ratios γ depend on the code rate (Tables 9 and 10); the point
//! orders were cross-checked against `leansdr`'s `sdr.h` and `dvb.h` (GPL-3).
//!
//! S2X adds 8/16/32APSK variants and 64/128/256APSK; those land with the S2X
//! MODCODs in M3.

use std::f32::consts::PI;

use decdvb_core::{CodeRate, Iq, Modulation};

/// A constellation: its points indexed by bits, and how many bits each carries.
#[derive(Debug, Clone, PartialEq)]
pub struct Constellation {
    pub modulation: Modulation,
    /// Points indexed by the symbol's bits, MSB first. Unit average power.
    pub points: Vec<Iq>,
    /// Ring radii, innermost first (one entry for PSK).
    pub rings: Vec<f32>,
}

/// A point at radius `r`, angle `i * 2π / n` — the same parametrisation the
/// standard's figures use, which keeps the tables below readable.
fn polar(r: f32, n: f32, i: f32) -> Iq {
    let a = i * 2.0 * PI / n;
    Iq::new(r * a.cos(), r * a.sin())
}

/// 16APSK inner/outer ring ratio γ for a code rate (EN 302 307-1 Table 9).
fn apsk16_gamma(rate: CodeRate) -> Option<f32> {
    Some(match (rate.num, rate.den) {
        (2, 3) => 3.15,
        (3, 4) => 2.85,
        (4, 5) => 2.75,
        (5, 6) => 2.70,
        (8, 9) => 2.60,
        (9, 10) => 2.57,
        _ => return None,
    })
}

/// 32APSK ring ratios (γ1, γ2) for a code rate (EN 302 307-1 Table 10).
fn apsk32_gammas(rate: CodeRate) -> Option<(f32, f32)> {
    Some(match (rate.num, rate.den) {
        (3, 4) => (2.84, 5.27),
        (4, 5) => (2.72, 4.87),
        (5, 6) => (2.64, 4.64),
        (8, 9) => (2.54, 4.33),
        (9, 10) => (2.53, 4.30),
        _ => return None,
    })
}

impl Constellation {
    /// Gray-mapped QPSK (§5.4.1).
    pub fn qpsk() -> Self {
        Constellation {
            modulation: Modulation::Qpsk,
            points: vec![
                polar(1.0, 4.0, 0.5), // 00 ->  π/4
                polar(1.0, 4.0, 3.5), // 01 -> -π/4
                polar(1.0, 4.0, 1.5), // 10 ->  3π/4
                polar(1.0, 4.0, 2.5), // 11 -> -3π/4
            ],
            rings: vec![1.0],
        }
    }

    /// Gray-mapped 8PSK (§5.4.2, Figure 9).
    pub fn psk8() -> Self {
        Constellation {
            modulation: Modulation::Psk8,
            points: vec![
                polar(1.0, 8.0, 1.0), // 000 ->  π/4
                polar(1.0, 8.0, 0.0), // 001 ->  0
                polar(1.0, 8.0, 4.0), // 010 ->  π
                polar(1.0, 8.0, 5.0), // 011 -> -3π/4
                polar(1.0, 8.0, 2.0), // 100 ->  π/2
                polar(1.0, 8.0, 7.0), // 101 -> -π/4
                polar(1.0, 8.0, 3.0), // 110 ->  3π/4
                polar(1.0, 8.0, 6.0), // 111 -> -π/2
            ],
            rings: vec![1.0],
        }
    }

    /// 4+12 16APSK with ring ratio γ (§5.4.3, Figure 10).
    pub fn apsk16(gamma: f32) -> Self {
        // Choose r1 so the average power is 1: (4 r1² + 12 r2²) / 16 = 1.
        let r1 = (4.0 / (1.0 + 3.0 * gamma * gamma)).sqrt();
        let r2 = gamma * r1;
        Constellation {
            modulation: Modulation::Apsk16,
            points: vec![
                polar(r2, 12.0, 1.5),
                polar(r2, 12.0, 10.5),
                polar(r2, 12.0, 4.5),
                polar(r2, 12.0, 7.5),
                polar(r2, 12.0, 0.5),
                polar(r2, 12.0, 11.5),
                polar(r2, 12.0, 5.5),
                polar(r2, 12.0, 6.5),
                polar(r2, 12.0, 2.5),
                polar(r2, 12.0, 9.5),
                polar(r2, 12.0, 3.5),
                polar(r2, 12.0, 8.5),
                polar(r1, 4.0, 0.5),
                polar(r1, 4.0, 3.5),
                polar(r1, 4.0, 1.5),
                polar(r1, 4.0, 2.5),
            ],
            rings: vec![r1, r2],
        }
    }

    /// 4+12+16 32APSK with ring ratios γ1, γ2 (§5.4.4, Figure 11).
    pub fn apsk32(gamma1: f32, gamma2: f32) -> Self {
        // (4 r1² + 12 r2² + 16 r3²) / 32 = 1.
        let r1 = (8.0 / (1.0 + 3.0 * gamma1 * gamma1 + 4.0 * gamma2 * gamma2)).sqrt();
        let r2 = gamma1 * r1;
        let r3 = gamma2 * r1;
        Constellation {
            modulation: Modulation::Apsk32,
            points: vec![
                polar(r2, 12.0, 1.5),
                polar(r2, 12.0, 2.5),
                polar(r2, 12.0, 10.5),
                polar(r2, 12.0, 9.5),
                polar(r2, 12.0, 4.5),
                polar(r2, 12.0, 3.5),
                polar(r2, 12.0, 7.5),
                polar(r2, 12.0, 8.5),
                polar(r3, 16.0, 1.0),
                polar(r3, 16.0, 3.0),
                polar(r3, 16.0, 14.0),
                polar(r3, 16.0, 12.0),
                polar(r3, 16.0, 6.0),
                polar(r3, 16.0, 4.0),
                polar(r3, 16.0, 9.0),
                polar(r3, 16.0, 11.0),
                polar(r2, 12.0, 0.5),
                polar(r1, 4.0, 0.5),
                polar(r2, 12.0, 11.5),
                polar(r1, 4.0, 3.5),
                polar(r2, 12.0, 5.5),
                polar(r1, 4.0, 1.5),
                polar(r2, 12.0, 6.5),
                polar(r1, 4.0, 2.5),
                polar(r3, 16.0, 0.0),
                polar(r3, 16.0, 2.0),
                polar(r3, 16.0, 15.0),
                polar(r3, 16.0, 13.0),
                polar(r3, 16.0, 7.0),
                polar(r3, 16.0, 5.0),
                polar(r3, 16.0, 8.0),
                polar(r3, 16.0, 10.0),
            ],
            rings: vec![r1, r2, r3],
        }
    }

    /// The constellation for a DVB-S2 MODCOD, or `None` for combinations the
    /// standard does not define (and, for now, for the S2X-only ones).
    pub fn for_modcod(modulation: Modulation, rate: CodeRate) -> Option<Self> {
        match modulation {
            Modulation::Qpsk => Some(Self::qpsk()),
            Modulation::Psk8 => Some(Self::psk8()),
            Modulation::Apsk16 => apsk16_gamma(rate).map(Self::apsk16),
            Modulation::Apsk32 => apsk32_gammas(rate).map(|(g1, g2)| Self::apsk32(g1, g2)),
            _ => None,
        }
    }

    /// Bits per symbol.
    pub fn bits(&self) -> u8 {
        self.modulation.bits_per_symbol()
    }

    /// Map a symbol's bits (MSB first, in the low `bits()` bits) to its point.
    pub fn map(&self, symbol_bits: usize) -> Iq {
        self.points[symbol_bits & (self.points.len() - 1)]
    }

    /// Nearest point's index (hard decision). Linear search: fine for display
    /// and identification; the M2 demapper computes soft LLRs instead.
    pub fn nearest(&self, x: Iq) -> usize {
        let mut best = 0;
        let mut best_d = f32::INFINITY;
        for (i, p) in self.points.iter().enumerate() {
            let d = (x - p).norm_sqr();
            if d < best_d {
                best_d = d;
                best = i;
            }
        }
        best
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_s2() -> Vec<Constellation> {
        let mut v = vec![Constellation::qpsk(), Constellation::psk8()];
        for (n, d) in [(2, 3), (3, 4), (4, 5), (5, 6), (8, 9), (9, 10)] {
            v.push(Constellation::for_modcod(Modulation::Apsk16, CodeRate::new(n, d)).unwrap());
        }
        for (n, d) in [(3, 4), (4, 5), (5, 6), (8, 9), (9, 10)] {
            v.push(Constellation::for_modcod(Modulation::Apsk32, CodeRate::new(n, d)).unwrap());
        }
        v
    }

    #[test]
    fn unit_average_power() {
        for c in all_s2() {
            let p: f32 = c.points.iter().map(|p| p.norm_sqr()).sum::<f32>() / c.points.len() as f32;
            assert!((p - 1.0).abs() < 1e-5, "{:?}: power {p}", c.modulation);
        }
    }

    #[test]
    fn point_count_matches_bits() {
        for c in all_s2() {
            assert_eq!(c.points.len(), 1 << c.bits(), "{:?}", c.modulation);
        }
    }

    #[test]
    fn points_are_distinct() {
        for c in all_s2() {
            for (i, a) in c.points.iter().enumerate() {
                for b in &c.points[i + 1..] {
                    assert!(
                        (a - b).norm() > 0.1,
                        "{:?}: coincident points",
                        c.modulation
                    );
                }
            }
        }
    }

    #[test]
    fn ring_populations_are_4_12_16() {
        let count_on = |c: &Constellation, r: f32| {
            c.points
                .iter()
                .filter(|p| (p.norm() - r).abs() < 1e-4)
                .count()
        };
        let c16 = Constellation::apsk16(2.85);
        assert_eq!(count_on(&c16, c16.rings[0]), 4);
        assert_eq!(count_on(&c16, c16.rings[1]), 12);
        let c32 = Constellation::apsk32(2.84, 5.27);
        assert_eq!(count_on(&c32, c32.rings[0]), 4);
        assert_eq!(count_on(&c32, c32.rings[1]), 12);
        assert_eq!(count_on(&c32, c32.rings[2]), 16);
    }

    #[test]
    fn ring_ratios_follow_gamma() {
        let c = Constellation::apsk32(2.64, 4.64);
        assert!((c.rings[1] / c.rings[0] - 2.64).abs() < 1e-4);
        assert!((c.rings[2] / c.rings[0] - 4.64).abs() < 1e-4);
    }

    #[test]
    fn psk_mappings_are_gray() {
        // Angularly adjacent points must differ in exactly one bit.
        for c in [Constellation::qpsk(), Constellation::psk8()] {
            let n = c.points.len();
            let mut by_angle: Vec<usize> = (0..n).collect();
            by_angle.sort_by(|&a, &b| c.points[a].arg().partial_cmp(&c.points[b].arg()).unwrap());
            for k in 0..n {
                let a = by_angle[k];
                let b = by_angle[(k + 1) % n];
                assert_eq!(
                    (a ^ b).count_ones(),
                    1,
                    "{:?}: {a:b} and {b:b} are neighbours",
                    c.modulation
                );
            }
        }
    }

    #[test]
    fn nearest_inverts_map() {
        for c in all_s2() {
            for i in 0..c.points.len() {
                assert_eq!(c.nearest(c.map(i)), i, "{:?}", c.modulation);
            }
        }
    }

    #[test]
    fn undefined_combinations_are_none() {
        // 16APSK has no rate-1/2 point in S2.
        assert!(Constellation::for_modcod(Modulation::Apsk16, CodeRate::new(1, 2)).is_none());
        assert!(Constellation::for_modcod(Modulation::Apsk32, CodeRate::new(2, 3)).is_none());
    }
}
