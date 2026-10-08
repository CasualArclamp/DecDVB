//! Streaming FIR filter with real taps and complex samples.

use decdvb_core::Iq;

/// A complex-in/complex-out FIR with real taps, holding state across calls so
/// it can be fed block by block without edge artefacts.
///
/// Rust note: the history is a fixed-length `Vec` used as a ring buffer; the
/// index wraps with `%`, which the compiler turns into a mask only for
/// power-of-two lengths, so this is written as an explicit compare instead.
pub struct Fir {
    taps: Vec<f32>,
    history: Vec<Iq>,
    pos: usize,
}

impl Fir {
    pub fn new(taps: Vec<f32>) -> Self {
        assert!(!taps.is_empty(), "a filter needs at least one tap");
        let n = taps.len();
        Fir {
            taps,
            history: vec![Iq::new(0.0, 0.0); n],
            pos: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.taps.len()
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    /// Group delay in samples (half the tap count for a symmetric filter).
    pub fn delay(&self) -> usize {
        self.taps.len() / 2
    }

    /// Push one sample and get the filtered output.
    pub fn push(&mut self, x: Iq) -> Iq {
        let n = self.taps.len();
        self.history[self.pos] = x;
        self.pos = if self.pos + 1 == n { 0 } else { self.pos + 1 };

        // Convolve: the newest sample multiplies taps[0].
        let mut acc = Iq::new(0.0, 0.0);
        let mut idx = self.pos;
        for &tap in self.taps.iter().rev() {
            acc += self.history[idx] * tap;
            idx = if idx + 1 == n { 0 } else { idx + 1 };
        }
        acc
    }

    /// Filter a whole block, appending to `out`.
    pub fn process(&mut self, input: &[Iq], out: &mut Vec<Iq>) {
        out.reserve(input.len());
        for &x in input {
            out.push(self.push(x));
        }
    }

    /// Clear the history (e.g. after a retune).
    pub fn reset(&mut self) {
        self.history.fill(Iq::new(0.0, 0.0));
        self.pos = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn impulse_response_is_the_taps() {
        let taps = vec![0.5, -0.25, 0.125];
        let mut f = Fir::new(taps.clone());
        let mut out = Vec::new();
        // An impulse followed by zeros reads the taps out in order.
        let input = [
            Iq::new(1.0, 0.0),
            Iq::new(0.0, 0.0),
            Iq::new(0.0, 0.0),
            Iq::new(0.0, 0.0),
        ];
        f.process(&input, &mut out);
        for (k, &tap) in taps.iter().enumerate() {
            assert!(
                (out[k].re - tap).abs() < 1e-6,
                "tap {k}: {} != {tap}",
                out[k].re
            );
        }
        assert!(out[3].norm() < 1e-6);
    }

    #[test]
    fn dc_gain_is_the_tap_sum() {
        let taps = vec![0.25f32; 4];
        let mut f = Fir::new(taps);
        let mut out = Vec::new();
        f.process(&[Iq::new(1.0, 0.0); 16], &mut out);
        // Settled output after the filter fills.
        assert!((out[15].re - 1.0).abs() < 1e-6, "{}", out[15].re);
    }

    #[test]
    fn imaginary_part_is_filtered_too() {
        let mut f = Fir::new(vec![1.0, 1.0]);
        let mut out = Vec::new();
        f.process(&[Iq::new(0.0, 1.0), Iq::new(0.0, 1.0)], &mut out);
        assert!((out[1].im - 2.0).abs() < 1e-6);
        assert!(out[1].re.abs() < 1e-6);
    }

    #[test]
    fn reset_clears_history() {
        let mut f = Fir::new(vec![1.0, 1.0, 1.0]);
        let mut out = Vec::new();
        f.process(&[Iq::new(1.0, 0.0); 3], &mut out);
        f.reset();
        out.clear();
        f.process(&[Iq::new(1.0, 0.0)], &mut out);
        // Only the new sample contributes.
        assert!((out[0].re - 1.0).abs() < 1e-6);
    }
}
