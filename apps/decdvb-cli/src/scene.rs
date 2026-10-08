//! `decdvb scene`: a wideband test capture with a mix of carriers, the kind
//! of span the waterfall, carrier detection and Identify are built for.
//!
//! At 8 MS/s (a typical HackRF setting), cs8 like a HackRF capture:
//!
//! | offset   | what                                             |
//! |----------|--------------------------------------------------|
//! | −2.5 MHz | DVB-S2 CCM, QPSK 1/2 + pilots, 1 MS/s, α 0.20    |
//! | −0.8 MHz | an unmodulated CW tone                           |
//! | +1.2 MHz | DVB-S2 **ACM**: QPSK 1/2 → 8PSK 3/5 → short 16APSK 2/3, 500 kS/s, α 0.25 |
//! | +2.8 MHz | plain QPSK, no PLHEADERs, 250 kS/s, α 0.35       |
//!
//! The DVB-S2 carriers are real PLFRAMEs (headers, pilots, scrambling); their
//! payload is random constellation points until the FEC encoder exists.

use std::path::Path;

use anyhow::Result;
use decdvb_core::{Iq, SampleFormat};
use decdvb_io::IqFileWriter;
use decdvb_mod::{FrameSpec, PlFramer, Shaper};

pub const RATE: f64 = 8e6;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn uniform(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn gauss(&mut self) -> Iq {
        let u1 = self.uniform().max(1e-300);
        let u2 = self.uniform();
        let r = (-2.0 * u1.ln()).sqrt();
        let t = std::f64::consts::TAU * u2;
        Iq::new((r * t.cos()) as f32, (r * t.sin()) as f32) * std::f32::consts::FRAC_1_SQRT_2
    }
}

/// A shaped carrier at `sps` samples/symbol, from `symbols`.
fn shaped(symbols: &[Iq], sps: usize, alpha: f64) -> Vec<Iq> {
    let mut sh = Shaper::new(sps, alpha, 16);
    let mut x = Vec::with_capacity(symbols.len() * sps);
    sh.process(symbols, &mut x);
    x
}

fn qpsk(n: usize, rng: &mut Rng) -> Vec<Iq> {
    let k = std::f32::consts::FRAC_1_SQRT_2;
    (0..n)
        .map(|_| match rng.next() >> 62 {
            0 => Iq::new(k, k),
            1 => Iq::new(-k, k),
            2 => Iq::new(-k, -k),
            _ => Iq::new(k, -k),
        })
        .collect()
}

/// Write the scene, `seconds` long.
pub fn write(out: &Path, seconds: f64) -> Result<()> {
    let n = (RATE * seconds) as usize;
    let mut rng = Rng(0x00DE_CD7B);

    let a = {
        let syms =
            PlFramer::new(0, 1).build_schedule(&[FrameSpec::new(4, false, true)], n / 8 + 64);
        shaped(&syms, 8, 0.20)
    };
    let b = {
        let sched = [
            FrameSpec::new(4, false, true),
            FrameSpec::new(12, false, true),
            FrameSpec::new(18, true, true),
        ];
        let syms = PlFramer::new(0, 2).build_schedule(&sched, n / 16 + 64);
        shaped(&syms, 16, 0.25)
    };
    let c = shaped(&qpsk(n / 32 + 64, &mut rng), 32, 0.35);

    // Mix: each carrier rotated to its offset and scaled to its level.
    let carriers: [(&[Iq], f64, f32); 3] =
        [(&a, -2.5e6, 0.20), (&b, 1.2e6, 0.16), (&c, 2.8e6, 0.12)];
    let cw_hz = -0.8e6;
    let noise = 0.035f32;

    let mut w = IqFileWriter::create(out, SampleFormat::Cs8)?;
    let mut block = Vec::with_capacity(1 << 16);
    let mut k = 0usize;
    while k < n {
        block.clear();
        let end = (k + (1 << 16)).min(n);
        for i in k..end {
            let t = i as f64 / RATE;
            let mut s = rng.gauss() * noise;
            for &(x, f, g) in &carriers {
                let ph = std::f64::consts::TAU * f * t;
                s += x[i] * Iq::new(ph.cos() as f32, ph.sin() as f32) * g;
            }
            let ph = std::f64::consts::TAU * cw_hz * t;
            s += Iq::new(ph.cos() as f32, ph.sin() as f32) * 0.05;
            block.push(s);
        }
        w.write(&block)?;
        k = end;
    }
    w.finish()?;
    Ok(())
}
