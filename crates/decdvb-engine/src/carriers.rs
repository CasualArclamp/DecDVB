//! Carrier detection across the whole wideband span.
//!
//! Finds every signal standing above the noise floor in an averaged spectrum
//! and estimates its centre, width and symbol rate, so the GUI can mark it on
//! the waterfall and let you claim it with one click — a VFO sized to fit.
//!
//! The symbol-rate figure is the equivalent-noise-bandwidth estimate (a few
//! percent); dropping a VFO with Identify on the carrier then pins it down to
//! well under a percent.

/// One detected carrier.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Carrier {
    /// Centre relative to the wideband centre, Hz.
    pub center_hz: f64,
    /// 99 % occupied bandwidth, Hz.
    pub bandwidth_hz: f64,
    /// Symbol-rate estimate from the equivalent noise bandwidth, Hz.
    pub symbol_rate_hz: f64,
    /// Peak above the noise floor, dB.
    pub snr_db: f32,
    /// Only a few bins wide: a CW tone, a spur, or a carrier too narrow to
    /// resolve at this FFT size.
    pub narrow: bool,
}

impl Carrier {
    /// A VFO bandwidth that comfortably fits the carrier: its occupied width
    /// plus margin for the skirts and some frequency error.
    pub fn suggested_vfo_bandwidth(&self) -> f64 {
        (self.bandwidth_hz * 1.25).max(self.symbol_rate_hz * 1.5)
    }
}

/// Bins either side of DC treated as the receiver's own DC spur (the HackRF has
/// a strong one) when nothing wider surrounds them.
const DC_GUARD_BINS: usize = 3;
/// Gaps up to this many bins inside a carrier do not split it (a deep notch in
/// a noisy average, or a pilot tone's neighbours).
const MERGE_GAP_BINS: usize = 2;
/// A run narrower than this is flagged `narrow`.
const NARROW_BINS: usize = 6;

/// `q`-quantile of `xs` (0..1), by sorting a copy.
fn quantile(xs: &[f32], q: f64) -> f32 {
    let mut v = xs.to_vec();
    v.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    v[((v.len() - 1) as f64 * q).round() as usize]
}

/// Detect carriers in an FFT-shifted dB spectrum (bin 0 = −rate/2).
///
/// `min_snr_db` is how far above the floor a carrier's peak must stand. The
/// floor is the 20th percentile rather than the median, so a busy span — a
/// transponder mostly full of carriers — still finds the true noise.
pub fn detect_carriers(spectrum_db: &[f32], rate: f64, min_snr_db: f32) -> Vec<Carrier> {
    let n = spectrum_db.len();
    if n < 16 {
        return Vec::new();
    }
    let bin_hz = rate / n as f64;
    let floor = quantile(spectrum_db, 0.2);
    let above = |k: usize, margin: f32| spectrum_db[k] > floor + margin;

    // Runs of bins above the detection threshold, merging small gaps.
    let mut runs: Vec<(usize, usize)> = Vec::new(); // inclusive
    let mut k = 0;
    while k < n {
        if above(k, min_snr_db) {
            let start = k;
            let mut end = k;
            let mut j = k + 1;
            while j < n {
                if above(j, min_snr_db) {
                    end = j;
                    j += 1;
                } else if j + MERGE_GAP_BINS < n
                    && (j..=j + MERGE_GAP_BINS).any(|g| above(g, min_snr_db))
                {
                    j += 1;
                } else {
                    break;
                }
            }
            runs.push((start, end));
            k = end + 1;
        } else {
            k += 1;
        }
    }

    // Grow each run outward over its skirts (anything 3 dB above the floor),
    // without running into its neighbour.
    let mut grown: Vec<(usize, usize)> = Vec::with_capacity(runs.len());
    for (idx, &(s, e)) in runs.iter().enumerate() {
        let left_limit = if idx == 0 { 0 } else { runs[idx - 1].1 + 1 };
        let right_limit = runs.get(idx + 1).map_or(n - 1, |r| r.0 - 1);
        let mut a = s;
        while a > left_limit && above(a - 1, 3.0) {
            a -= 1;
        }
        let mut b = e;
        while b < right_limit && above(b + 1, 3.0) {
            b += 1;
        }
        grown.push((a, b));
    }

    let floor_lin = 10f64.powf(floor as f64 / 10.0);
    let dc = n / 2;
    let mut out = Vec::new();
    for (a, b) in grown {
        let width = b - a + 1;
        // The DC spur on its own is not a carrier.
        if width <= 2 * DC_GUARD_BINS + 1 && a + DC_GUARD_BINS >= dc && b <= dc + DC_GUARD_BINS {
            continue;
        }

        let excess: Vec<f64> = (a..=b)
            .map(|k| (10f64.powf(spectrum_db[k] as f64 / 10.0) - floor_lin).max(0.0))
            .collect();
        let total: f64 = excess.iter().sum();
        if total <= 0.0 {
            continue;
        }
        let freq = |k: usize| (k as f64 / n as f64 - 0.5) * rate;
        let center = excess
            .iter()
            .enumerate()
            .map(|(i, &p)| p * freq(a + i))
            .sum::<f64>()
            / total;

        // Narrowest window holding 99 % of the excess power.
        let target = 0.99 * total;
        let (mut lo, mut acc, mut best) = (0usize, 0.0f64, excess.len());
        for hi in 0..excess.len() {
            acc += excess[hi];
            while lo < hi && acc - excess[lo] >= target {
                acc -= excess[lo];
                lo += 1;
            }
            if acc >= target {
                best = best.min(hi - lo + 1);
            }
        }

        // Equivalent noise bandwidth over the plateau.
        let max_e = excess.iter().copied().fold(0.0, f64::max);
        let mut plateau: Vec<f64> = excess
            .iter()
            .copied()
            .filter(|&p| p >= 0.5 * max_e)
            .collect();
        plateau.sort_unstable_by(|x, y| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal));
        let p0 = plateau[plateau.len() / 2];
        let rs = if p0 > 0.0 { total * bin_hz / p0 } else { 0.0 };

        let peak = spectrum_db[a..=b].iter().copied().fold(f32::MIN, f32::max);
        out.push(Carrier {
            center_hz: center,
            bandwidth_hz: best as f64 * bin_hz,
            symbol_rate_hz: rs,
            snr_db: peak - floor,
            narrow: width < NARROW_BINS,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A spectrum with raised-cosine carriers `(centre Hz, Rs, alpha, level dB)`
    /// over a flat floor, plus a DC spur.
    fn spectrum(n: usize, rate: f64, carriers: &[(f64, f64, f64, f32)], dc_spur: bool) -> Vec<f32> {
        let floor = -80.0f32;
        let mut lin: Vec<f64> = vec![10f64.powf(floor as f64 / 10.0); n];
        for &(c, rs, alpha, level) in carriers {
            let p0 = 10f64.powf(level as f64 / 10.0);
            for (k, v) in lin.iter_mut().enumerate() {
                let f = (k as f64 / n as f64 - 0.5) * rate - c;
                let a = f.abs();
                let flat = (1.0 - alpha) * rs / 2.0;
                let edge = (1.0 + alpha) * rs / 2.0;
                let shape = if a <= flat {
                    1.0
                } else if a <= edge {
                    0.5 * (1.0 + (std::f64::consts::PI / (alpha * rs) * (a - flat)).cos())
                } else {
                    0.0
                };
                *v += p0 * shape;
            }
        }
        if dc_spur {
            lin[n / 2] += 10f64.powf(-40.0 / 10.0);
        }
        lin.iter().map(|&p| (10.0 * p.log10()) as f32).collect()
    }

    #[test]
    fn finds_two_carriers_and_ignores_the_dc_spur() {
        let rate = 20e6;
        let n = 8192;
        let s = spectrum(
            n,
            rate,
            &[(-4.0e6, 2.0e6, 0.20, -50.0), (3.5e6, 500e3, 0.35, -55.0)],
            true,
        );
        let found = detect_carriers(&s, rate, 6.0);
        assert_eq!(found.len(), 2, "{found:#?}");

        let bin = rate / n as f64;
        let a = found[0];
        assert!((a.center_hz + 4.0e6).abs() < 3.0 * bin, "{a:?}");
        assert!((a.symbol_rate_hz - 2.0e6).abs() / 2.0e6 < 0.03, "{a:?}");
        assert!(!a.narrow);

        let b = found[1];
        assert!((b.center_hz - 3.5e6).abs() < 3.0 * bin, "{b:?}");
        assert!((b.symbol_rate_hz - 500e3).abs() / 500e3 < 0.05, "{b:?}");
        assert!(b.snr_db > 20.0);
    }

    #[test]
    fn adjacent_carriers_stay_separate() {
        // Two carriers with a small guard band: a busy transponder.
        let rate = 10e6;
        let n = 8192;
        let s = spectrum(
            n,
            rate,
            &[(-0.7e6, 1.0e6, 0.20, -50.0), (0.7e6, 1.0e6, 0.20, -50.0)],
            false,
        );
        let found = detect_carriers(&s, rate, 6.0);
        assert_eq!(found.len(), 2, "{found:#?}");
    }

    #[test]
    fn cw_tone_is_flagged_narrow() {
        let rate = 2e6;
        let n = 4096;
        let mut s = spectrum(n, rate, &[], false);
        s[3000] = -30.0; // a tone well away from DC
        let found = detect_carriers(&s, rate, 6.0);
        assert_eq!(found.len(), 1);
        assert!(found[0].narrow);
    }

    #[test]
    fn empty_span_finds_nothing() {
        let s = spectrum(4096, 2e6, &[], true);
        assert!(detect_carriers(&s, 2e6, 6.0).is_empty());
    }

    #[test]
    fn suggested_vfo_fits_the_carrier() {
        let c = Carrier {
            center_hz: 0.0,
            bandwidth_hz: 1.15e6,
            symbol_rate_hz: 1.0e6,
            snr_db: 20.0,
            narrow: false,
        };
        assert!(c.suggested_vfo_bandwidth() >= 1.35e6);
    }
}
