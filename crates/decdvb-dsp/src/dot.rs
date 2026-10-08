//! The inner loop of every FIR here: a complex-by-real dot product.
//!
//! Written with eight independent partial sums because the compiler will not
//! vectorise a single floating-point accumulator — it may not reorder float
//! additions, and one running sum is a serial dependency chain. Eight lanes of
//! re/im sums break the chain; the result differs from the serial sum only by
//! float rounding.
//!
//! Measured on the 9800X3D (116 taps, release): serial 4.0, this 5.0 G MAC/s.
//! Two alternatives were tried and measured slower, so they are not here:
//! duplicated taps over a flat `f32` view of the samples, with AVX2/FMA picked
//! by run-time dispatch (3.5 G MAC/s — the per-call dispatch cost more than the
//! wider vectors saved on windows this short). The serial loop is faster than
//! its dependency chain suggests because successive outputs are independent and
//! the CPU overlaps them out of order. Bigger wins have to come from doing less
//! work (multi-stage decimation), not a faster inner loop.

use decdvb_core::Iq;

/// `Σ x[k] · h[k]` for complex `x` and real `h` of equal length.
#[inline]
pub fn dot_cr(x: &[Iq], h: &[f32]) -> Iq {
    debug_assert_eq!(x.len(), h.len());
    let n = x.len().min(h.len());
    let (x, h) = (&x[..n], &h[..n]);

    let mut re = [0.0f32; 8];
    let mut im = [0.0f32; 8];
    // Rust note: `as_chunks::<8>()` yields `&[T; 8]` arrays, so the inner loop
    // has a constant trip count with no bounds checks — what SIMD wants.
    let (xc, xr) = x.as_chunks::<8>();
    let (hc, hr) = h.as_chunks::<8>();
    for (xs, hs) in xc.iter().zip(hc) {
        for k in 0..8 {
            re[k] += xs[k].re * hs[k];
            im[k] += xs[k].im * hs[k];
        }
    }
    let mut acc = Iq::new(re.iter().sum(), im.iter().sum());
    for (s, &t) in xr.iter().zip(hr) {
        acc += s * t;
    }
    acc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_serial_sum() {
        for n in [0usize, 1, 7, 8, 9, 63, 64, 65, 1000] {
            let x: Vec<Iq> = (0..n)
                .map(|k| Iq::new((k as f32 * 0.37).sin(), (k as f32 * 0.11).cos()))
                .collect();
            let h: Vec<f32> = (0..n)
                .map(|k| (k as f32 * 0.23).cos() / (1.0 + k as f32))
                .collect();
            let serial = x
                .iter()
                .zip(&h)
                .fold(Iq::new(0.0, 0.0), |a, (s, &t)| a + s * t);
            let fast = dot_cr(&x, &h);
            assert!(
                (serial - fast).norm() < 1e-4 * (1.0 + serial.norm()),
                "n {n}: {serial} vs {fast}"
            );
        }
    }
}
