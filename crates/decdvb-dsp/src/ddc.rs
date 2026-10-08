//! Digital down-converter: the front of every VFO.
//!
//! Takes the wideband stream, shifts the VFO's centre frequency to DC with a
//! numerically controlled oscillator, low-pass filters to the VFO's bandwidth
//! and decimates by an integer factor. The output rate is chosen to leave at
//! least ~2.5 samples per symbol for timing recovery; the exact samples per
//! symbol is then fractional, which the symbol synchroniser's interpolator
//! handles, so no fractional resampler is needed here.
//!
//! The filter is a Blackman-windowed sinc. Its stopband starts at
//! `out_rate − bw/2`, the lowest frequency that would alias back into the
//! passband after decimation, so the whole transition band can be spent between
//! the passband edge and that point.

use decdvb_core::Iq;

/// Output rate as a multiple of the VFO bandwidth (before rounding the
/// decimation factor down, which only raises it).
const OVERSAMPLE: f64 = 2.5;
/// Blackman window: transition width ≈ 5.5 / N (normalised), ~74 dB stopband.
const BLACKMAN_K: f64 = 5.5;
/// Upper bound on filter length, so a very wide VFO near the full span cannot
/// ask for an absurd filter. Past this the filter just gets softer.
const MAX_TAPS: usize = 2047;
/// Renormalise the NCO phasor this often to stop magnitude drift.
const RENORM_EVERY: u32 = 1024;

/// Design a Blackman-windowed-sinc low-pass filter.
///
/// `cutoff` is the −6 dB point as a fraction of the sample rate (0..0.5).
/// Unity DC gain. Odd length, linear phase.
pub fn lowpass(cutoff: f64, ntaps: usize) -> Vec<f32> {
    let ntaps = ntaps | 1; // odd, so there is a centre tap
    let m = (ntaps - 1) as f64;
    let mut taps: Vec<f64> = (0..ntaps)
        .map(|k| {
            let n = k as f64 - m / 2.0;
            let sinc = if n == 0.0 {
                2.0 * cutoff
            } else {
                (2.0 * std::f64::consts::PI * cutoff * n).sin() / (std::f64::consts::PI * n)
            };
            let w = 0.42 - 0.5 * (2.0 * std::f64::consts::PI * k as f64 / m).cos()
                + 0.08 * (4.0 * std::f64::consts::PI * k as f64 / m).cos();
            sinc * w
        })
        .collect();
    let sum: f64 = taps.iter().sum();
    for t in &mut taps {
        *t /= sum;
    }
    taps.into_iter().map(|t| t as f32).collect()
}

/// Numerically controlled oscillator mixing + decimating FIR.
pub struct Ddc {
    in_rate: f64,
    decim: usize,
    bandwidth: f64,
    offset_hz: f64,
    /// Current oscillator phasor and its per-sample step (unit magnitude).
    /// f64 so that hours of running do not accumulate audible phase error.
    phasor: (f64, f64),
    step: (f64, f64),
    since_renorm: u32,
    /// Filter taps, reversed so the dot product runs over the buffer forwards.
    taps_rev: Vec<f32>,
    /// Mixed samples: the last `taps - 1` from previous calls, then new ones.
    buf: Vec<Iq>,
    /// Index in `buf` of the input sample at which the next output is due.
    next_out: usize,
}

impl Ddc {
    /// A DDC for a VFO centred `offset_hz` from the wideband centre, `bandwidth`
    /// wide, fed at `in_rate`.
    ///
    /// # Panics
    /// If `bandwidth` is not positive or exceeds `in_rate`.
    pub fn new(in_rate: f64, offset_hz: f64, bandwidth: f64) -> Self {
        assert!(
            bandwidth > 0.0 && bandwidth <= in_rate,
            "VFO bandwidth must be positive and within the sample rate"
        );
        let decim = ((in_rate / (OVERSAMPLE * bandwidth)).floor() as usize).max(1);
        let out_rate = in_rate / decim as f64;

        // Transition band: from the passband edge to where aliasing would start.
        // With decim == 1 there is no aliasing, so just roll off past the edge.
        let transition = if decim > 1 {
            (out_rate - bandwidth).max(0.05 * bandwidth)
        } else {
            (in_rate - bandwidth)
                .max(0.05 * bandwidth)
                .min(in_rate / 2.0)
        };
        let ntaps = ((BLACKMAN_K * in_rate / transition).ceil() as usize).clamp(15, MAX_TAPS);
        // Cut off midway through the transition band.
        let cutoff = ((bandwidth / 2.0 + transition / 2.0) / in_rate).min(0.49);

        let mut taps_rev = lowpass(cutoff, ntaps);
        taps_rev.reverse();
        let l = taps_rev.len();

        let mut ddc = Ddc {
            in_rate,
            decim,
            bandwidth,
            offset_hz: 0.0,
            phasor: (1.0, 0.0),
            step: (1.0, 0.0),
            since_renorm: 0,
            taps_rev,
            buf: vec![Iq::new(0.0, 0.0); l - 1],
            next_out: l - 1,
        };
        ddc.set_offset(offset_hz);
        ddc
    }

    /// Retune without disturbing the filter state or the oscillator phase, so a
    /// VFO can be dragged across the waterfall smoothly.
    pub fn set_offset(&mut self, offset_hz: f64) {
        self.offset_hz = offset_hz;
        // Mixing *down* by the offset: multiply by exp(-j 2π f t).
        let w = -2.0 * std::f64::consts::PI * offset_hz / self.in_rate;
        self.step = (w.cos(), w.sin());
    }

    pub fn offset_hz(&self) -> f64 {
        self.offset_hz
    }

    pub fn bandwidth(&self) -> f64 {
        self.bandwidth
    }

    pub fn decimation(&self) -> usize {
        self.decim
    }

    pub fn out_rate(&self) -> f64 {
        self.in_rate / self.decim as f64
    }

    pub fn taps(&self) -> usize {
        self.taps_rev.len()
    }

    /// Mix, filter and decimate `input`, appending to `out`.
    pub fn process(&mut self, input: &[Iq], out: &mut Vec<Iq>) {
        let l = self.taps_rev.len();

        // Mix every input sample down and append it to the filter buffer.
        self.buf.reserve(input.len());
        let (sr, si) = self.step;
        for &x in input {
            let (pr, pi) = self.phasor;
            let m = Iq::new(
                (x.re as f64 * pr - x.im as f64 * pi) as f32,
                (x.re as f64 * pi + x.im as f64 * pr) as f32,
            );
            self.buf.push(m);
            self.phasor = (pr * sr - pi * si, pr * si + pi * sr);
            self.since_renorm += 1;
            if self.since_renorm == RENORM_EVERY {
                let mag = (self.phasor.0 * self.phasor.0 + self.phasor.1 * self.phasor.1).sqrt();
                self.phasor = (self.phasor.0 / mag, self.phasor.1 / mag);
                self.since_renorm = 0;
            }
        }

        // One filter output per `decim` inputs, each over the `l` samples
        // ending at `next_out`.
        out.reserve((self.buf.len() - self.next_out) / self.decim + 1);
        while self.next_out < self.buf.len() {
            let window = &self.buf[self.next_out + 1 - l..=self.next_out];
            let mut acc = Iq::new(0.0, 0.0);
            for (s, &t) in window.iter().zip(&self.taps_rev) {
                acc += s * t;
            }
            out.push(acc);
            self.next_out += self.decim;
        }

        // Keep only the history the next call needs.
        let drop = self.buf.len() - (l - 1);
        self.buf.drain(..drop);
        self.next_out -= drop;
    }

    /// Clear filter history (e.g. after the source restarts).
    pub fn reset(&mut self) {
        let l = self.taps_rev.len();
        self.buf.clear();
        self.buf.resize(l - 1, Iq::new(0.0, 0.0));
        self.next_out = l - 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::TAU;

    fn tone(rate: f64, freq: f64, n: usize) -> Vec<Iq> {
        (0..n)
            .map(|k| {
                let ph = TAU * freq * k as f64 / rate;
                Iq::new(ph.cos() as f32, ph.sin() as f32)
            })
            .collect()
    }

    fn power_db(x: &[Iq]) -> f64 {
        let p: f64 = x.iter().map(|s| s.norm_sqr() as f64).sum::<f64>() / x.len() as f64;
        10.0 * p.max(1e-30).log10()
    }

    #[test]
    fn lowpass_has_unity_dc_gain() {
        let t = lowpass(0.1, 101);
        let s: f32 = t.iter().sum();
        assert!((s - 1.0).abs() < 1e-5);
        assert_eq!(t.len() % 2, 1);
    }

    #[test]
    fn decimation_targets_the_oversample_factor() {
        let d = Ddc::new(20e6, 1e6, 500e3);
        // 20e6 / (2.5 * 500e3) = 16.
        assert_eq!(d.decimation(), 16);
        assert!((d.out_rate() - 1.25e6).abs() < 1.0);
    }

    #[test]
    fn tone_at_the_vfo_centre_comes_out_at_dc() {
        let rate = 8e6;
        let off = 1.234e6;
        let mut d = Ddc::new(rate, off, 400e3);
        let mut out = Vec::new();
        d.process(&tone(rate, off, 200_000), &mut out);

        // Skip the filter's start-up transient, then the output should be a
        // constant phasor: tiny sample-to-sample phase change.
        let settled = &out[d.taps() / d.decimation() + 10..];
        let max_step = settled
            .windows(2)
            .map(|w| (w[1] * w[0].conj()).arg().abs())
            .fold(0.0f32, f32::max);
        assert!(max_step < 1e-3, "residual rotation {max_step} rad/sample");
        // And at unity gain.
        assert!(
            power_db(settled).abs() < 0.1,
            "gain {} dB",
            power_db(settled)
        );
    }

    #[test]
    fn out_of_band_tone_is_rejected() {
        let rate = 8e6;
        let mut d = Ddc::new(rate, 0.0, 400e3);
        let mut out = Vec::new();
        // 1.5 MHz away: far outside the 200 kHz half-bandwidth, and it would
        // alias into the passband if the filter let it through.
        d.process(&tone(rate, 1.5e6, 400_000), &mut out);
        let settled = &out[d.taps() / d.decimation() + 10..];
        assert!(power_db(settled) < -60.0, "leak {} dB", power_db(settled));
    }

    #[test]
    fn block_size_does_not_change_the_output() {
        // A VFO is fed whatever block size the source produces, so the result
        // must not depend on how the stream was chopped up.
        let rate = 4e6;
        let input = tone(rate, 333e3, 50_000);

        let mut a = Ddc::new(rate, 300e3, 200e3);
        let mut one = Vec::new();
        a.process(&input, &mut one);

        let mut b = Ddc::new(rate, 300e3, 200e3);
        let mut many = Vec::new();
        for chunk in input.chunks(777) {
            b.process(chunk, &mut many);
        }

        assert_eq!(one.len(), many.len());
        for (x, y) in one.iter().zip(&many) {
            assert!((x - y).norm() < 1e-4, "{x} vs {y}");
        }
    }

    #[test]
    fn retuning_moves_the_passband() {
        let rate = 4e6;
        let mut d = Ddc::new(rate, 0.0, 200e3);
        let mut out = Vec::new();
        d.set_offset(1e6);
        d.process(&tone(rate, 1e6, 100_000), &mut out);
        let settled = &out[d.taps() / d.decimation() + 10..];
        assert!(power_db(settled).abs() < 0.2);
        assert_eq!(d.offset_hz(), 1e6);
    }

    #[test]
    fn output_count_matches_decimation() {
        let mut d = Ddc::new(10e6, 0.0, 1e6);
        let mut out = Vec::new();
        d.process(&vec![Iq::new(1.0, 0.0); 40_000], &mut out);
        assert_eq!(out.len(), 40_000 / d.decimation());
    }
}
