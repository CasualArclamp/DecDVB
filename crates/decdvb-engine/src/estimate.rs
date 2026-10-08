//! Coarse signal estimates read off the power spectrum.
//!
//! These answer "where is the carrier and how wide is it?" without any
//! knowledge of the waveform, which is the first step of blind acquisition:
//! the centre frequency seeds the carrier-recovery loop and the occupied
//! bandwidth seeds the symbol-rate guess (for a roll-off α, the occupied
//! bandwidth is about `(1 + α)` times the symbol rate).

/// Where the energy in a spectrum sits.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BandEstimate {
    /// Power-weighted centre of the occupied band, in Hz relative to DC.
    pub center_hz: f64,
    /// Width of the narrowest contiguous band holding the requested fraction
    /// of the above-floor power, in Hz.
    pub bandwidth_hz: f64,
    /// Noise floor estimate (median bin), in dB.
    pub floor_db: f32,
    /// Highest bin, in dB.
    pub peak_db: f32,
    /// Peak-to-floor ratio in dB — a rough "is there a signal here" score.
    pub snr_db: f32,
    /// Symbol-rate estimate in symbols/s, from the equivalent noise bandwidth.
    ///
    /// A root-raised-cosine signal has a *raised-cosine* power spectrum: flat at
    /// `P0` over `|f| < (1-α)Rs/2`, then a cosine-squared skirt out to
    /// `(1+α)Rs/2`. Integrating that gives `∫P df = P0 · Rs` for **any** roll-off,
    /// because the two halves of the skirt sum to one flat region of width `αRs`.
    /// So `∫P df / P0` recovers the symbol rate without knowing α — much better
    /// than scaling an occupied bandwidth by a guessed `(1 + α)`.
    pub symbol_rate_hz: f64,
}

/// Estimate the occupied band from a dB spectrum produced by
/// [`crate::Spectrum::compute`] (FFT-shifted, so bin 0 is `-rate/2`).
///
/// `fraction` is the share of above-floor power the band must contain; 0.99 is
/// a good default. Returns `None` for an empty spectrum.
pub fn estimate_band(spectrum_db: &[f32], sample_rate: f64, fraction: f64) -> Option<BandEstimate> {
    let n = spectrum_db.len();
    if n == 0 {
        return None;
    }

    // Median as the noise floor: robust to a strong carrier, unlike the mean.
    let mut sorted: Vec<f32> = spectrum_db.to_vec();
    sorted.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let floor_db = sorted[n / 2];
    let peak_db = *sorted.last().unwrap();

    // Work in linear power with the floor removed, so noise bins contribute ~0
    // and the centroid is not dragged towards the middle of the window.
    let excess: Vec<f64> = spectrum_db
        .iter()
        .map(|&db| {
            let lin = 10f64.powf(db as f64 / 10.0);
            let fl = 10f64.powf(floor_db as f64 / 10.0);
            (lin - fl).max(0.0)
        })
        .collect();

    let total: f64 = excess.iter().sum();
    let bin_hz = sample_rate / n as f64;
    // Bin k is at (k/n - 0.5) * rate.
    let bin_freq = |k: usize| (k as f64 / n as f64 - 0.5) * sample_rate;

    if total <= 0.0 {
        // Nothing above the floor: no signal.
        return Some(BandEstimate {
            center_hz: 0.0,
            bandwidth_hz: 0.0,
            floor_db,
            peak_db,
            snr_db: peak_db - floor_db,
            symbol_rate_hz: 0.0,
        });
    }

    let center_hz = excess
        .iter()
        .enumerate()
        .map(|(k, &p)| p * bin_freq(k))
        .sum::<f64>()
        / total;

    // Narrowest contiguous run of bins holding `fraction` of the power, found
    // with a two-pointer sweep over the running sum.
    let target = total * fraction.clamp(0.0, 1.0);
    let mut best = n;
    let mut lo = 0usize;
    let mut acc = 0.0f64;
    for hi in 0..n {
        acc += excess[hi];
        while acc - excess[lo] >= target && lo < hi {
            acc -= excess[lo];
            lo += 1;
        }
        if acc >= target {
            best = best.min(hi - lo + 1);
        }
    }

    // Equivalent noise bandwidth: the integral over the plateau level. `P0` is
    // the median of the bins in the top half of the dynamic range rather than
    // the maximum, so a single noisy bin cannot shrink the estimate.
    let max_excess = excess.iter().copied().fold(0.0f64, f64::max);
    let mut plateau: Vec<f64> = excess
        .iter()
        .copied()
        .filter(|&p| p >= 0.5 * max_excess)
        .collect();
    let symbol_rate_hz = if plateau.is_empty() {
        0.0
    } else {
        plateau.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let p0 = plateau[plateau.len() / 2];
        if p0 > 0.0 { total * bin_hz / p0 } else { 0.0 }
    };

    Some(BandEstimate {
        center_hz,
        bandwidth_hz: best as f64 * bin_hz,
        floor_db,
        peak_db,
        snr_db: peak_db - floor_db,
        symbol_rate_hz,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a dB spectrum with a flat band of `width` bins centred on `center`.
    fn synth(n: usize, center: usize, width: usize, signal_db: f32, floor_db: f32) -> Vec<f32> {
        let mut s = vec![floor_db; n];
        let lo = center - width / 2;
        s[lo..lo + width].fill(signal_db);
        s
    }

    #[test]
    fn finds_a_centred_band() {
        let n = 1024;
        let rate = 2_000_000.0;
        // Centred on DC (bin n/2), 128 bins wide.
        let s = synth(n, n / 2, 128, -10.0, -60.0);
        let e = estimate_band(&s, rate, 0.99).unwrap();
        assert!(e.center_hz.abs() < rate / n as f64, "{}", e.center_hz);
        let expected_bw = 128.0 * rate / n as f64;
        assert!(
            (e.bandwidth_hz - expected_bw).abs() < 2.0 * rate / n as f64,
            "{} vs {expected_bw}",
            e.bandwidth_hz
        );
        assert!((e.snr_db - 50.0).abs() < 0.1);
    }

    #[test]
    fn finds_an_offset_band() {
        let n = 2048;
        let rate = 2_000_000.0;
        // 600 kHz wide at +150 kHz, as `decdvb synth` produces by default.
        let bin_hz = rate / n as f64;
        let width = (600_000.0 / bin_hz) as usize;
        let center_bin = n / 2 + (150_000.0 / bin_hz) as usize;
        let s = synth(n, center_bin, width, -5.0, -55.0);

        let e = estimate_band(&s, rate, 0.99).unwrap();
        assert!(
            (e.center_hz - 150_000.0).abs() < 3.0 * bin_hz,
            "centre {} Hz",
            e.center_hz
        );
        // A 0.99 fraction of a rectangular band is 99 % of its width.
        let expected = 0.99 * 600_000.0;
        assert!(
            (e.bandwidth_hz - expected).abs() < 4.0 * bin_hz,
            "bandwidth {} Hz, expected ~{expected}",
            e.bandwidth_hz
        );
    }

    #[test]
    fn enbw_recovers_the_width_of_a_flat_band() {
        // For a rectangular spectrum the equivalent noise bandwidth is just the
        // band's width — the degenerate alpha -> 0 case of the RRC relation.
        let n = 2048;
        let rate = 2_000_000.0;
        let bin_hz = rate / n as f64;
        let width = 512;
        let s = synth(n, n / 2, width, -5.0, -65.0);
        let e = estimate_band(&s, rate, 0.99).unwrap();
        let expected = width as f64 * bin_hz;
        assert!(
            (e.symbol_rate_hz - expected).abs() < 0.03 * expected,
            "Rs {} vs {expected}",
            e.symbol_rate_hz
        );
    }

    #[test]
    fn enbw_is_independent_of_roll_off() {
        // Build raised-cosine power spectra for the same Rs at two very
        // different roll-offs; the ENBW estimate must agree with Rs for both.
        let n = 4096;
        let rate = 4_000_000.0;
        let bin_hz = rate / n as f64;
        let rs = 1_000_000.0f64;

        for &alpha in &[0.05f64, 0.35] {
            let mut db = vec![-70.0f32; n];
            for (k, slot) in db.iter_mut().enumerate() {
                let f = ((k as f64 / n as f64) - 0.5) * rate;
                let a = f.abs();
                let flat = (1.0 - alpha) * rs / 2.0;
                let edge = (1.0 + alpha) * rs / 2.0;
                // Raised-cosine power response, normalised to a 0 dB plateau.
                let p = if a <= flat {
                    1.0
                } else if a <= edge {
                    let x = std::f64::consts::PI / (alpha * rs) * (a - flat);
                    0.5 * (1.0 + (x).cos())
                } else {
                    0.0
                };
                if p > 0.0 {
                    *slot = (10.0 * p.log10()) as f32;
                }
            }
            let e = estimate_band(&db, rate, 0.99).unwrap();
            assert!(
                (e.symbol_rate_hz - rs).abs() < 0.05 * rs,
                "alpha {alpha}: Rs {} vs {rs} (bin {bin_hz})",
                e.symbol_rate_hz
            );
        }
    }

    #[test]
    fn flat_noise_reports_no_signal() {
        let s = vec![-60.0f32; 512];
        let e = estimate_band(&s, 1e6, 0.99).unwrap();
        assert_eq!(e.bandwidth_hz, 0.0);
        assert!(e.snr_db.abs() < 0.1);
    }

    #[test]
    fn empty_is_none() {
        assert!(estimate_band(&[], 1e6, 0.99).is_none());
    }
}
