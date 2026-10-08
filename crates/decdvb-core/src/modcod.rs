//! Modulation and coding definitions.
//!
//! The 28 standard DVB-S2 MODCODs (ETSI EN 302 307-1, used in the PLS code) are
//! tabulated here. DVB-S2X adds many more MODCODs, VL-SNR modes and the medium
//! FECFRAME; those are filled in at milestone M3 (see `docs/DESIGN.md`).

use std::fmt;

/// Constellation / modulation order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Modulation {
    /// Plain BPSK — not a DVB-S2 modulation, but common on generic carriers
    /// (telemetry, SCPC data), so the generic PSK decoder handles it.
    Bpsk,
    /// pi/2-BPSK (S2X VL-SNR).
    Pi2Bpsk,
    Qpsk,
    Psk8,
    Apsk16,
    Apsk32,
    Apsk64,
    Apsk128,
    Apsk256,
}

impl Modulation {
    /// Bits carried per channel symbol.
    pub const fn bits_per_symbol(self) -> u8 {
        match self {
            Modulation::Bpsk | Modulation::Pi2Bpsk => 1,
            Modulation::Qpsk => 2,
            Modulation::Psk8 => 3,
            Modulation::Apsk16 => 4,
            Modulation::Apsk32 => 5,
            Modulation::Apsk64 => 6,
            Modulation::Apsk128 => 7,
            Modulation::Apsk256 => 8,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Modulation::Bpsk => "BPSK",
            Modulation::Pi2Bpsk => "pi/2-BPSK",
            Modulation::Qpsk => "QPSK",
            Modulation::Psk8 => "8PSK",
            Modulation::Apsk16 => "16APSK",
            Modulation::Apsk32 => "32APSK",
            Modulation::Apsk64 => "64APSK",
            Modulation::Apsk128 => "128APSK",
            Modulation::Apsk256 => "256APSK",
        }
    }
}

impl fmt::Display for Modulation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// LDPC code rate as an exact fraction. Stored as numerator/denominator because
/// S2X uses rates (13/45, 9/20, 11/20, 26/45, …) that are awkward as an enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CodeRate {
    pub num: u16,
    pub den: u16,
}

impl CodeRate {
    pub const fn new(num: u16, den: u16) -> Self {
        CodeRate { num, den }
    }

    /// Rate as a float in (0, 1).
    pub fn as_f64(self) -> f64 {
        self.num as f64 / self.den as f64
    }
}

impl fmt::Display for CodeRate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.num, self.den)
    }
}

/// FECFRAME length in coded bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FecFrame {
    /// 64 800 bits.
    Normal,
    /// 16 200 bits.
    Short,
    /// 32 400 bits (S2X only).
    Medium,
}

impl FecFrame {
    /// LDPC codeword length in bits.
    pub const fn n_ldpc(self) -> usize {
        match self {
            FecFrame::Normal => 64_800,
            FecFrame::Short => 16_200,
            FecFrame::Medium => 32_400,
        }
    }
}

/// One modulation-and-coding point: what the PLHEADER announces for a PLFRAME.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Modcod {
    /// Index as signalled in the PLS code (S2: 1..=28; dummy frame = 0).
    pub index: u8,
    pub modulation: Modulation,
    pub rate: CodeRate,
    pub frame: FecFrame,
}

impl Modcod {
    /// Information bits per FECFRAME (K_ldpc = N * rate), the BBFRAME size minus
    /// the BCH parity. Convenience for throughput display; exact K values come
    /// from the FEC tables in `decdvb-fec`.
    pub fn k_approx(self) -> usize {
        (self.frame.n_ldpc() as f64 * self.rate.as_f64()).round() as usize
    }
}

impl fmt::Display for Modcod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.modulation, self.rate)
    }
}

const fn mc(index: u8, m: Modulation, num: u16, den: u16) -> Modcod {
    Modcod {
        index,
        modulation: m,
        rate: CodeRate::new(num, den),
        frame: FecFrame::Normal,
    }
}

/// The 28 standard DVB-S2 MODCODs for the normal FECFRAME, indexed 1..=28 as in
/// the PLS code (EN 302 307-1, Table 12). Short-frame variants share the same
/// modulation/rate with `FecFrame::Short`.
///
/// Rust note: this is a `static` rather than an array built inside the accessor,
/// because returning `&'static [T]` needs the data to outlive the call. `mc` is a
/// `const fn`, so the whole table is built at compile time.
static S2_MODCODS: [Modcod; 28] = {
    use Modulation::*;
    [
        mc(1, Qpsk, 1, 4),
        mc(2, Qpsk, 1, 3),
        mc(3, Qpsk, 2, 5),
        mc(4, Qpsk, 1, 2),
        mc(5, Qpsk, 3, 5),
        mc(6, Qpsk, 2, 3),
        mc(7, Qpsk, 3, 4),
        mc(8, Qpsk, 4, 5),
        mc(9, Qpsk, 5, 6),
        mc(10, Qpsk, 8, 9),
        mc(11, Qpsk, 9, 10),
        mc(12, Psk8, 3, 5),
        mc(13, Psk8, 2, 3),
        mc(14, Psk8, 3, 4),
        mc(15, Psk8, 5, 6),
        mc(16, Psk8, 8, 9),
        mc(17, Psk8, 9, 10),
        mc(18, Apsk16, 2, 3),
        mc(19, Apsk16, 3, 4),
        mc(20, Apsk16, 4, 5),
        mc(21, Apsk16, 5, 6),
        mc(22, Apsk16, 8, 9),
        mc(23, Apsk16, 9, 10),
        mc(24, Apsk32, 3, 4),
        mc(25, Apsk32, 4, 5),
        mc(26, Apsk32, 5, 6),
        mc(27, Apsk32, 8, 9),
        mc(28, Apsk32, 9, 10),
    ]
};

/// The standard DVB-S2 MODCOD table (see [`S2_MODCODS`]).
pub fn s2_modcod_table() -> &'static [Modcod] {
    &S2_MODCODS
}

/// Look up a standard S2 MODCOD by PLS index (1..=28) for the given frame length.
pub fn s2_modcod(index: u8, frame: FecFrame) -> Option<Modcod> {
    s2_modcod_table()
        .iter()
        .find(|m| m.index == index)
        .map(|m| Modcod { frame, ..*m })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_28_and_monotonic() {
        let t = s2_modcod_table();
        assert_eq!(t.len(), 28);
        for (i, m) in t.iter().enumerate() {
            assert_eq!(m.index as usize, i + 1);
        }
    }

    #[test]
    fn k_approx_qpsk_half() {
        let m = s2_modcod(4, FecFrame::Normal).unwrap(); // QPSK 1/2
        assert_eq!(m.k_approx(), 32_400);
        assert_eq!(m.modulation.bits_per_symbol(), 2);
    }

    #[test]
    fn display_is_human() {
        let m = s2_modcod(14, FecFrame::Normal).unwrap(); // 8PSK 3/4
        assert_eq!(m.to_string(), "8PSK 3/4");
    }
}
