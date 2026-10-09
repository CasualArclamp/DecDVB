//! In-app playback and recording of multicast audio (radio over IP over
//! DVB-S/S2: GSE or MPE).
//!
//! - [`depay`]: a stream's UDP payloads to whole frames (RTP, RFC 2250,
//!   RFC 3016 LATM, RFC 3640, raw ADTS/LOAS/MPEG audio, PCM).
//! - [`decode`]: frames to stereo PCM (Symphonia: MPEG audio I–III, AAC-LC;
//!   libopus: Opus).
//! - [`resample`]: to the sound card's rate, drift-trimmed.
//! - [`player`]: the thread and sound output, with volume and pause.
//! - [`record`]: the stream to a file as broadcast.
//!
//! HE-AAC plays as its AAC-LC core (Symphonia has no SBR); open the stream in
//! VLC for full bandwidth.

pub mod aac;
pub mod bits;
pub mod decode;
pub mod depay;
pub mod es;
pub mod player;
pub mod record;
pub mod resample;

pub use player::{
    AudioHandle, AudioPlayer, OutputKind, PlayState, Status, muted, set_muted, set_volume, volume,
};
pub use record::AudioRecorder;
