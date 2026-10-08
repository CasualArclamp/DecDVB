//! DSP building blocks for DecDVB.
//!
//! M0 provides the root-raised-cosine filter and a streaming FIR, which the
//! modulator and the receive matched filter both need. Timing recovery, carrier
//! recovery and the resampler land in M1 — see `docs/DESIGN.md`.

pub mod fir;
pub mod rrc;

pub use fir::Fir;
pub use rrc::rrc_taps;
