//! DVB-S2 modulator.
//!
//! M1b provides the physical layer: PLFRAME construction (PLHEADER, payload,
//! pilots, scrambling) and pulse shaping. Payload symbols are random points
//! on the right constellation until the FEC encoder exists (M2), which makes
//! the output statistically and spectrally identical to a real carrier — all
//! that acquisition and identification can see — but not decodable past the
//! PL layer. The full encode chain arrives in M6.

pub mod framer;
pub mod shaper;

pub use framer::{FrameSpec, PlFramer};
pub use shaper::Shaper;
