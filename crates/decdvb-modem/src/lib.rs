//! Satellite modem formats beyond DVB-S2: DVB-S (EN 300 421) and Intelsat
//! IESS-315's turbo product code `tpc_2964`; the IESS Viterbi/Reed–Solomon
//! modes after.
//!
//! - [`conv`]: the K = 7 convolutional code, puncturing, Viterbi decoding.
//! - [`rs`]: shortened Reed–Solomon codes over GF(256).
//! - [`interleave`]: Forney convolutional interleaving.
//! - [`dvbs`]: the DVB-S transmitter and blind receiver.
//! - [`tpc`]: turbo product codes (extended Hamming products, Chase–Pyndiah).
//! - [`tpc2964`]: `tpc_2964` frames, their structure found from the signal.
//! - [`payload`]: what a modem's data carry (HDLC, MPEG-TS) and how they are
//!   scrambled, found from the data.
//! - [`text`]: live text search in any bit stream, every reading at once.

pub mod conv;
pub mod dvbs;
pub mod e1;
pub mod interleave;
pub mod payload;
pub mod rs;
pub mod text;
pub mod tpc;
pub mod tpc2964;
