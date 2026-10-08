//! TS-mode BBFRAMEs to MPEG-TS, what the stream carries, and where it goes.
//!
//! - [`deframe`]: data fields back to 188-byte packets (CRC-8 chain lock,
//!   null-packet re-insertion).
//! - [`psi`]: per-PID counts and continuity; PAT, PMT and SDT for the
//!   programmes and service names.
//! - [`sink`]: a `.ts` file, UDP, or a TCP/HTTP server for VLC and PotPlayer.

pub mod deframe;
pub mod psi;
pub mod sink;

pub use deframe::{DeframeStats, TS_LEN, TsDeframer, null_packet};
pub use psi::{EsInfo, PidStats, Programme, TsAnalyser, stream_type_name};
pub use sink::{TcpSink, TsFile, UdpSink};
