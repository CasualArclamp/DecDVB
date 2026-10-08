//! DC (zero-frequency spike) removal for a wideband IQ stream.
//!
//! Direct-conversion receivers like the HackRF leave a DC offset — LO leakage
//! and ADC/mixer offsets — that shows as a spike at the centre frequency. It is
//! nearly static, so subtracting the stream's mean, tracked block by block and
//! smoothed, removes it at the cost of one sum and one subtraction per sample.
//! The smoothing makes the notch only a few hertz wide at waterfall block
//! rates; a carrier sitting exactly on the centre frequency loses only its
//! own DC component.

use decdvb_core::Iq;

/// Weight of each new block's mean in the running estimate.
const SMOOTHING: f64 = 0.3;

/// Running-mean DC remover.
#[derive(Debug, Clone, Default)]
pub struct DcBlocker {
    mean: Option<(f64, f64)>,
}

impl DcBlocker {
    pub fn new() -> Self {
        Self::default()
    }

    /// The DC offset currently being removed.
    pub fn offset(&self) -> Iq {
        let (re, im) = self.mean.unwrap_or((0.0, 0.0));
        Iq::new(re as f32, im as f32)
    }

    /// Update the estimate from `block` and subtract it, in place.
    pub fn process(&mut self, block: &mut [Iq]) {
        if block.is_empty() {
            return;
        }
        // f64 sums: millions of f32 samples would lose the small mean.
        let (sr, si) = block
            .iter()
            .fold((0f64, 0f64), |(r, i), s| (r + s.re as f64, i + s.im as f64));
        let n = block.len() as f64;
        let m = (sr / n, si / n);
        let (re, im) = match self.mean {
            Some((r, i)) => (r + SMOOTHING * (m.0 - r), i + SMOOTHING * (m.1 - i)),
            None => m,
        };
        self.mean = Some((re, im));
        let dc = Iq::new(re as f32, im as f32);
        for s in block.iter_mut() {
            *s -= dc;
        }
    }

    /// Forget the estimate (a retune or a new source moves the offset).
    pub fn reset(&mut self) {
        self.mean = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_a_static_offset_and_leaves_a_tone() {
        let dc = Iq::new(0.03, -0.02);
        let w = std::f64::consts::TAU * 0.01;
        let mut b = DcBlocker::new();
        let mut last = Vec::new();
        for k in 0..10 {
            let mut block: Vec<Iq> = (0..100_000)
                .map(|n| {
                    let ph = w * (k * 100_000 + n) as f64;
                    Iq::new(ph.cos() as f32, ph.sin() as f32) * 0.1 + dc
                })
                .collect();
            b.process(&mut block);
            last = block;
        }
        assert!((b.offset() - dc).norm() < 1e-4, "offset {}", b.offset());
        // What is left has no mean, and the tone is untouched.
        let mean = last.iter().sum::<Iq>() / last.len() as f32;
        assert!(mean.norm() < 1e-4, "mean {mean}");
        let p = last.iter().map(|s| s.norm_sqr()).sum::<f32>() / last.len() as f32;
        assert!((p - 0.01).abs() < 1e-4, "tone power {p}");
    }
}
