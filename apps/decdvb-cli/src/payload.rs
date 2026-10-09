//! `decdvb payload`: a modem data recording (the `.bin` a FastLink, TPC or
//! Viterbi VFO writes: its descrambled data, MSB first) run through the
//! payload stage again — the format found, Paradise framing, and the 257-bit
//! TDM multiplex inside it — with every aligned 2 ms frame written out for
//! analysis if asked (`--tdm-out`: 257 bytes of 0/1 a frame, the alignment
//! bit first, then the sixteen 16-bit words).

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Args;
use decdvb_modem::payload::{Format, PayloadOut, PayloadRx};
use decdvb_modem::tdm257::TdmRx;

#[derive(Args)]
pub struct PayloadArgs {
    /// A data recording (.bin).
    pub file: PathBuf,
    /// Data bits a modem frame: 16384 for FastLink (the default), 2223 for
    /// TPC 2964, 4096 for Viterbi.
    #[arg(long, default_value_t = 16_384)]
    pub frame_bits: usize,
    /// Write the TDM multiplex's aligned frames here.
    #[arg(long)]
    pub tdm_out: Option<PathBuf>,
}

pub fn run(a: &PayloadArgs) -> Result<()> {
    let bytes = std::fs::read(&a.file).with_context(|| format!("reading {}", a.file.display()))?;
    // MSB first, as the recorder packs them.
    let bits: Vec<u8> = bytes
        .iter()
        .flat_map(|&b| (0..8).rev().map(move |k| (b >> k) & 1))
        .collect();
    let mut pay = PayloadRx::new(a.frame_bits);
    let mut out = PayloadOut::default();
    let mut tdm = TdmRx::new();
    tdm.keep_frames = a.tdm_out.is_some();
    let mut esc_bits = 0usize;
    for frame in bits.chunks_exact(a.frame_bits) {
        out.clear();
        pay.push(frame, &mut out);
        esc_bits += out.esc.len();
        if matches!(pay.format(), Some(Format::ParadiseEsc)) && !out.inner.is_empty() {
            tdm.push(&out.inner);
        }
    }
    println!(
        "{}: {} frames of {} bits ({:.1} s at 128 kbit/s)",
        a.file.display(),
        bits.len() / a.frame_bits,
        a.frame_bits,
        bits.len() as f64 / 128e3
    );
    crate::decode::print_payload(&pay.stats);
    if esc_bits > 0 {
        println!("  ESC: {esc_bits} bits");
    }
    if tdm.stats.frames > 0 || tdm.stats.locked {
        crate::decode::print_tdm(&tdm.stats);
    }
    if let Some(p) = &a.tdm_out {
        std::fs::write(p, &tdm.frames_out).with_context(|| format!("writing {}", p.display()))?;
        println!(
            "wrote {} TDM frames to {}",
            tdm.frames_out.len() / decdvb_modem::tdm257::FRAME,
            p.display()
        );
    }
    Ok(())
}
