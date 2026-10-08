//! DSP building blocks for DecDVB.
//!
//! - [`rrc`] / [`fir`]: pulse shaping and the matched filter.
//! - [`ddc`]: the digital down-converter at the front of every VFO.
//! - [`agc`]: block AGC to unit power.
//! - [`timing`]: Gardner symbol synchroniser with a cubic interpolator.
//!
//! Carrier recovery lands with the rest of M1 — see `docs/DESIGN.md`.

pub mod agc;
pub mod ddc;
pub mod fir;
pub mod rrc;
pub mod timing;

pub use agc::Agc;
pub use ddc::{Ddc, lowpass};
pub use fir::Fir;
pub use rrc::rrc_taps;
pub use timing::SymbolSync;
