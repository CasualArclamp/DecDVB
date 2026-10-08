//! Live HackRF One front end.
//!
//! Scaffolding only (M0). The implementation lands in M1 and will load
//! `libhackrf` at run time with `libloading` rather than linking it, so that
//! building DecDVB never requires the HackRF SDK and CI stays green on a
//! machine with no SDR attached.
//!
//! Planned bindings: `hackrf_init`, `hackrf_open`, `hackrf_set_sample_rate`,
//! `hackrf_set_freq`, `hackrf_set_lna_gain`, `hackrf_set_vga_gain`,
//! `hackrf_set_amp_enable`, `hackrf_start_rx` (callback into a ring buffer),
//! `hackrf_stop_rx`, `hackrf_close`, `hackrf_exit`.
//!
//! The device delivers `cs8` at up to ~20 MS/s over USB 2.0; DecDVB's useful
//! ceiling is therefore a symbol rate near 15 MS/s.

use decdvb_core::{Error, Iq, Result};

use crate::IqSource;

/// Gain settings for the HackRF's three stages.
#[derive(Debug, Clone, Copy)]
pub struct HackRfGains {
    /// RF amplifier (0 or 14 dB).
    pub amp: bool,
    /// IF/LNA gain, 0–40 dB in 8 dB steps.
    pub lna_db: u8,
    /// Baseband/VGA gain, 0–62 dB in 2 dB steps.
    pub vga_db: u8,
}

impl Default for HackRfGains {
    fn default() -> Self {
        HackRfGains {
            amp: false,
            lna_db: 24,
            vga_db: 20,
        }
    }
}

/// A live HackRF One receive stream.
pub struct HackRfSource {
    sample_rate: f64,
    center_freq: f64,
}

impl HackRfSource {
    /// Open the first HackRF and start receiving.
    pub fn open(sample_rate: f64, center_freq: f64, _gains: HackRfGains) -> Result<Self> {
        let _ = (sample_rate, center_freq);
        Err(Error::Unimplemented(
            "live HackRF capture (lands in milestone M1); use an IQ file for now",
        ))
    }
}

impl IqSource for HackRfSource {
    fn read(&mut self, _out: &mut Vec<Iq>) -> Result<usize> {
        Err(Error::Unimplemented("live HackRF capture"))
    }

    fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    fn center_freq(&self) -> f64 {
        self.center_freq
    }

    fn describe(&self) -> String {
        format!(
            "HackRF One @ {:.3} MHz, {:.3} MS/s",
            self.center_freq / 1e6,
            self.sample_rate / 1e6
        )
    }
}
