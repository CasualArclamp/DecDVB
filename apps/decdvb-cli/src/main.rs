//! DecDVB command line.
//!
//! M0 provides `modcods` (print the coding tables) and `analyse` (read an IQ
//! file and report level/spectrum occupancy). `decode` and `modulate` arrive
//! with the later milestones.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use decdvb_core::{FecFrame, RxConfig, SampleFormat, s2_modcod_table};
use decdvb_engine::{Receiver, estimate_band};
use decdvb_io::{IqFileReader, IqSource, format_from_path};

#[derive(Parser)]
#[command(
    name = "decdvb",
    version,
    about = "DVB-S2/S2X receiver with GSE/IP de-encapsulation"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print the modulation and coding table.
    Modcods,
    /// Read an IQ file and report basic signal statistics.
    Analyse {
        /// IQ capture to read.
        file: PathBuf,
        /// Sample format; inferred from the extension when omitted.
        #[arg(long, value_parser = parse_format)]
        format: Option<SampleFormat>,
        /// Sample rate in samples/s (e.g. 2e6 or 2000000).
        #[arg(long, default_value_t = 2_000_000.0)]
        rate: f64,
        /// Stop after this many blocks (0 = whole file).
        #[arg(long, default_value_t = 8)]
        blocks: usize,
    },
    /// Write a synthetic RRC-shaped QPSK capture, for testing the chain.
    ///
    /// This is a signal generator, not yet a DVB-S2 modulator: it has no
    /// PLFRAMEs or FEC. It exists so the display and acquisition stages have
    /// something with the right spectral shape to chew on. The real modulator
    /// arrives in M6.
    Synth {
        /// Output file (`.cs8`, `.cs16` or `.cf32` picks the format).
        out: PathBuf,
        /// Sample rate in samples/s.
        #[arg(long, default_value_t = 2_000_000.0)]
        rate: f64,
        /// Symbol rate in symbols/s.
        #[arg(long, default_value_t = 500_000.0)]
        symbol_rate: f64,
        /// Roll-off factor.
        #[arg(long, default_value_t = 0.20)]
        alpha: f64,
        /// Carrier frequency offset in Hz.
        #[arg(long, default_value_t = 0.0)]
        offset: f64,
        /// Es/N0 in dB (very large = no noise).
        #[arg(long, default_value_t = 12.0)]
        esn0: f64,
        /// Duration in seconds.
        #[arg(long, default_value_t = 1.0)]
        seconds: f64,
        /// PRNG seed, so a capture is reproducible.
        #[arg(long, default_value_t = 1)]
        seed: u64,
    },
}

fn parse_format(s: &str) -> std::result::Result<SampleFormat, String> {
    match s.to_ascii_lowercase().as_str() {
        "cs8" | "s8" | "i8" => Ok(SampleFormat::Cs8),
        "cs16" | "s16" => Ok(SampleFormat::Cs16),
        "cf32" | "fc32" | "f32" => Ok(SampleFormat::Cf32),
        other => Err(format!("unknown format `{other}` (want cs8, cs16 or cf32)")),
    }
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Modcods => modcods(),
        Command::Analyse {
            file,
            format,
            rate,
            blocks,
        } => analyse(file, format, rate, blocks),
        Command::Synth {
            out,
            rate,
            symbol_rate,
            alpha,
            offset,
            esn0,
            seconds,
            seed,
        } => synth(SynthArgs {
            out,
            rate,
            symbol_rate,
            alpha,
            offset,
            esn0,
            seconds,
            seed,
        }),
    }
}

struct SynthArgs {
    out: PathBuf,
    rate: f64,
    symbol_rate: f64,
    alpha: f64,
    offset: f64,
    esn0: f64,
    seconds: f64,
    seed: u64,
}

/// xorshift64*: a few lines, reproducible, and good enough for test signals.
/// (Not for anything that needs cryptographic or statistical rigour.)
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in [0, 1).
    fn next_f64(&mut self) -> f64 {
        // Top 53 bits give a double with full mantissa precision.
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// One standard normal pair, Box–Muller.
    fn next_gaussian_pair(&mut self) -> (f64, f64) {
        let u1 = self.next_f64().max(f64::MIN_POSITIVE);
        let u2 = self.next_f64();
        let r = (-2.0 * u1.ln()).sqrt();
        let th = std::f64::consts::TAU * u2;
        (r * th.cos(), r * th.sin())
    }
}

fn synth(a: SynthArgs) -> Result<()> {
    use decdvb_core::Iq;
    use decdvb_dsp::{Fir, rrc_taps};
    use decdvb_io::IqFileWriter;

    let fmt = format_from_path(&a.out).unwrap_or(SampleFormat::Cs8);
    let sps_exact = a.rate / a.symbol_rate;
    let sps = sps_exact.round();
    if (sps_exact - sps).abs() > 1e-6 {
        bail!(
            "rate / symbol_rate must be a whole number for now (got {sps_exact:.4}); \
             fractional resampling arrives in M1"
        );
    }
    if sps < 2.0 {
        bail!("need at least 2 samples per symbol (got {sps})");
    }
    let sps_i = sps as usize;

    let mut rng = Rng(a.seed | 1);
    let mut shaper = Fir::new(rrc_taps(sps, a.alpha, 16));
    let mut writer = IqFileWriter::create(&a.out, fmt)?;

    let total_symbols = (a.seconds * a.symbol_rate).round() as usize;
    // Es/N0 -> per-sample noise sigma. Signal power is normalised to 1 below.
    let n0 = 10f64.powf(-a.esn0 / 10.0);
    let sigma = (n0 / 2.0).sqrt();
    // Keep well clear of full scale so cs8 does not clip on peaks.
    let amplitude = 0.35;
    let dphi = std::f64::consts::TAU * a.offset / a.rate;

    let mut phase = 0.0f64;
    let mut block: Vec<Iq> = Vec::with_capacity(sps_i * 1024);
    let mut shaped: Vec<Iq> = Vec::with_capacity(sps_i * 1024);

    for chunk_start in (0..total_symbols).step_by(1024) {
        let n = 1024.min(total_symbols - chunk_start);
        block.clear();
        for _ in 0..n {
            // Gray-mapped QPSK on the diagonals, unit average power.
            let bits = self_next_two_bits(&mut rng);
            let s = match bits {
                0 => Iq::new(
                    std::f32::consts::FRAC_1_SQRT_2,
                    std::f32::consts::FRAC_1_SQRT_2,
                ),
                1 => Iq::new(
                    -std::f32::consts::FRAC_1_SQRT_2,
                    std::f32::consts::FRAC_1_SQRT_2,
                ),
                2 => Iq::new(
                    -std::f32::consts::FRAC_1_SQRT_2,
                    -std::f32::consts::FRAC_1_SQRT_2,
                ),
                _ => Iq::new(
                    std::f32::consts::FRAC_1_SQRT_2,
                    -std::f32::consts::FRAC_1_SQRT_2,
                ),
            };
            // Upsample: the symbol, then sps-1 zeros for the shaper to fill in.
            block.push(s);
            for _ in 1..sps_i {
                block.push(Iq::new(0.0, 0.0));
            }
        }

        shaped.clear();
        shaper.process(&block, &mut shaped);

        // Zero-stuffing divides the power by sps; sqrt(sps) puts it back.
        let gain = (amplitude * sps.sqrt()) as f32;
        for s in shaped.iter_mut() {
            let (nr, ni) = rng.next_gaussian_pair();
            let noise = Iq::new((sigma * nr) as f32, (sigma * ni) as f32) * amplitude as f32;
            let rot = Iq::new(phase.cos() as f32, phase.sin() as f32);
            *s = *s * gain * rot + noise;
            phase += dphi;
            if phase > std::f64::consts::TAU {
                phase -= std::f64::consts::TAU;
            }
        }
        writer.write(&shaped)?;
    }

    writer.finish()?;
    println!(
        "wrote {} — {:.3} s, {:.0} S/s, {:.0} sym/s ({} sps), alpha {:.2}, \
         offset {:+.0} Hz, Es/N0 {:.1} dB, {:?}",
        a.out.display(),
        a.seconds,
        a.rate,
        a.symbol_rate,
        sps_i,
        a.alpha,
        a.offset,
        a.esn0,
        fmt
    );
    Ok(())
}

/// Two fresh bits from the generator (one QPSK symbol).
fn self_next_two_bits(rng: &mut Rng) -> u8 {
    (rng.next_u64() & 0b11) as u8
}

fn modcods() -> Result<()> {
    println!(
        "{:>3}  {:<10} {:>6}  {:>9}  {:>9}",
        "idx", "mod", "rate", "K(64800)", "K(16200)"
    );
    for m in s2_modcod_table() {
        let short = decdvb_core::modcod::s2_modcod(m.index, FecFrame::Short)
            .map(|s| s.k_approx())
            .unwrap_or(0);
        println!(
            "{:>3}  {:<10} {:>6}  {:>9}  {:>9}",
            m.index,
            m.modulation.name(),
            m.rate.to_string(),
            m.k_approx(),
            short
        );
    }
    println!("\n28 DVB-S2 MODCODs. S2X additions land in milestone M3.");
    Ok(())
}

fn analyse(file: PathBuf, format: Option<SampleFormat>, rate: f64, blocks: usize) -> Result<()> {
    let fmt = match format.or_else(|| format_from_path(&file)) {
        Some(f) => f,
        None => bail!(
            "cannot tell the sample format of {} from its extension — pass --format cs8|cs16|cf32",
            file.display()
        ),
    };

    const FFT: usize = 4096;
    let reader = IqFileReader::open(&file, fmt, rate, FFT * 16)
        .with_context(|| format!("opening {}", file.display()))?;
    println!("source: {}", reader.describe());

    let mut rx = Receiver::new(
        Box::new(reader),
        RxConfig {
            sample_rate: rate,
            ..Default::default()
        },
        FFT,
    );

    let mut total = 0usize;
    let mut block = 0usize;
    let mut last: Option<decdvb_engine::BandEstimate> = None;
    while let Some(snap) = rx.step()? {
        total += snap.samples;
        block += 1;

        let rms = (snap.scatter.iter().map(|s| s.norm_sqr()).sum::<f32>()
            / snap.scatter.len().max(1) as f32)
            .sqrt();

        match estimate_band(&snap.spectrum_db, rate, 0.99) {
            Some(b) => {
                println!(
                    "block {block:>4}: rms {rms:>7.4}  centre {:>+8.1} kHz  \
                     bw(99%) {:>7.1} kHz  Rs ~{:>7.1} kS/s  S/N {:>5.1} dB",
                    b.center_hz / 1e3,
                    b.bandwidth_hz / 1e3,
                    b.symbol_rate_hz / 1e3,
                    b.snr_db,
                );
                last = Some(b);
            }
            None => println!("block {block:>4}: rms {rms:>7.4}  (no spectrum)"),
        }

        if blocks != 0 && block >= blocks {
            break;
        }
    }

    println!(
        "read {total} samples ({:.3} s at {:.3} MS/s)",
        total as f64 / rate,
        rate / 1e6
    );
    if let Some(b) = last {
        // Deliberately no roll-off estimate here: a 99 % occupied bandwidth is
        // narrower than the true (1 + alpha) * Rs width, because the RRC skirts
        // carry little power, so alpha derived from it reads systematically low
        // (measured ~0.14 for a true 0.20). M1 identifies the roll-off properly
        // by sweeping matched filters and taking the best timing-error metric.
        println!(
            "suggest: --symbol-rate {:.0} --offset {:.0}; \
             blind acquisition in M1 will refine this",
            b.symbol_rate_hz, b.center_hz,
        );
    }
    Ok(())
}
