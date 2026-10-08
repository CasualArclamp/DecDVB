//! Forward error correction and constellation mapping for DVB-S2/S2X.
//!
//! M1b provides the S2 constellations (points and bit mapping). The soft
//! demapper, bit interleaver, LDPC and BCH codecs arrive in M2–M3 — see
//! `docs/DESIGN.md`.

pub mod constellation;

pub use constellation::Constellation;
