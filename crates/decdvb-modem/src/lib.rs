//! Satellite modem formats beyond DVB-S2: DVB-S (EN 300 421) now; the
//! Intelsat (IESS) Viterbi/Reed–Solomon and turbo-product-code modes after.
//!
//! - [`conv`]: the K = 7 convolutional code, puncturing, Viterbi decoding.
//! - [`rs`]: shortened Reed–Solomon codes over GF(256).
//! - [`interleave`]: Forney convolutional interleaving.
//! - [`dvbs`]: the DVB-S transmitter and blind receiver.

pub mod conv;
pub mod dvbs;
pub mod interleave;
pub mod rs;
