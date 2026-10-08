//! Physical-layer framing for DVB-S2/S2X.
//!
//! M1 covers the PLHEADER: the 26-symbol SOF, the 64-symbol PLS code and the
//! pi/2-BPSK mapping both are carried in, plus the PLFRAME geometry (slots,
//! pilot blocks) that the PLS code implies. BBFRAME/BBHEADER parsing and
//! superframing follow in M2 and M5 — see `docs/DESIGN.md`.

pub mod defs;
pub mod pi2bpsk;
pub mod plsc;
pub mod rm;
pub mod scramble;
pub mod sync;

pub use defs::*;
pub use plsc::{PlsInfo, PlscDecoder, PlscDemap, PlscEncoder};
pub use rm::ReedMuller;
pub use scramble::PlScrambler;
pub use sync::PlHeaderCorrelator;
