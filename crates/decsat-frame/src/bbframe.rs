//! BBFRAMEs: the baseband header, its CRC-8, and BB scrambling
//! (EN 302 307-1 §5.1.4, §5.1.6, §5.2.2).
//!
//! A BBFRAME is K_bch bits: the 80-bit BBHEADER, the data field (DFL bits),
//! then padding. The whole frame, header included, is BB-scrambled.

use std::sync::OnceLock;

use decsat_core::RollOff;

/// BBHEADER length in bytes.
pub const BBHEADER_LEN: usize = 10;

/// CRC-8 of §5.1.4: g(x) = x⁸ + x⁷ + x⁶ + x⁴ + x² + 1, MSB first, register
/// starting at 0, no final XOR. Used for the BBHEADER and, in TS mode, for
/// each user packet.
pub fn crc8(bytes: &[u8]) -> u8 {
    static TABLE: OnceLock<[u8; 256]> = OnceLock::new();
    let t = TABLE.get_or_init(|| {
        let mut t = [0u8; 256];
        for (v, e) in t.iter_mut().enumerate() {
            let mut c = v as u8;
            for _ in 0..8 {
                c = if c & 0x80 != 0 {
                    (c << 1) ^ 0xD5
                } else {
                    c << 1
                };
            }
            *e = c;
        }
        t
    });
    bytes.iter().fold(0u8, |c, &b| t[(c ^ b) as usize])
}

/// The BB scrambling sequence for the longest BBFRAME (58 192 bits).
fn bb_sequence() -> &'static [u8] {
    static SEQ: OnceLock<Vec<u8>> = OnceLock::new();
    SEQ.get_or_init(|| {
        // 1 + x¹⁴ + x¹⁵, stages 1..15 held in bits 14..0, loaded with
        // 100101010000000 at the start of every BBFRAME (Figure 5). The
        // output is stage 14 ⊕ stage 15, fed back into stage 1.
        let mut sr: u16 = 0x4A80;
        let mut seq = vec![0u8; 58_192 / 8];
        for i in 0..seq.len() * 8 {
            let b = ((sr ^ (sr >> 1)) & 1) as u8;
            seq[i / 8] |= b << (7 - i % 8);
            sr >>= 1;
            if b == 1 {
                sr |= 0x4000;
            }
        }
        seq
    })
}

/// BB-scramble (or descramble — it is its own inverse) a whole BBFRAME.
pub fn bb_scramble(frame: &mut [u8]) {
    for (b, s) in frame.iter_mut().zip(bb_sequence()) {
        *b ^= s;
    }
}

/// The TS/GS field of MATYPE-1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamFormat {
    /// 00: generic packetized.
    GenericPacketized,
    /// 01: generic continuous — the carrier of GSE.
    GenericContinuous,
    /// 10: reserved in EN 302 307-1.
    Reserved,
    /// 11: MPEG transport stream.
    Transport,
}

impl StreamFormat {
    pub fn label(self) -> &'static str {
        match self {
            StreamFormat::GenericPacketized => "generic packetized",
            StreamFormat::GenericContinuous => "generic continuous (GSE)",
            StreamFormat::Reserved => "reserved (10)",
            StreamFormat::Transport => "transport stream",
        }
    }
}

/// A parsed BBHEADER (§5.1.6, Figure 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BbHeader {
    pub format: StreamFormat,
    /// SIS (true) or MIS.
    pub single_stream: bool,
    /// CCM (true) or ACM/VCM.
    pub ccm: bool,
    /// Input stream synchronisation indicator.
    pub issyi: bool,
    /// Null-packet deletion.
    pub npd: bool,
    /// The transmission roll-off; `None` for the reserved code 11.
    pub roll_off: Option<RollOff>,
    /// MATYPE-2: the input stream identifier when MIS.
    pub isi: u8,
    /// User packet length, bits (0 for continuous streams).
    pub upl: u16,
    /// Data field length, bits.
    pub dfl: u16,
    /// The user packet sync byte.
    pub sync: u8,
    /// Bits from the data field start to the first user packet's start.
    pub syncd: u16,
    /// High-efficiency mode: signalled by the CRC-8 field holding CRC ⊕ 1
    /// (normal mode: the CRC itself).
    pub high_efficiency: bool,
}

/// Why a BBHEADER was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BbHeaderError {
    #[error("BBHEADER CRC-8 mismatch")]
    Crc,
    #[error("BBFRAME shorter than a BBHEADER")]
    Short,
}

impl BbHeader {
    /// Parse and check the first 10 bytes of a (descrambled) BBFRAME.
    pub fn parse(frame: &[u8]) -> Result<Self, BbHeaderError> {
        if frame.len() < BBHEADER_LEN {
            return Err(BbHeaderError::Short);
        }
        let h = &frame[..BBHEADER_LEN];
        let crc = crc8(&h[..9]);
        let high_efficiency = match h[9] ^ crc {
            0 => false,
            1 => true,
            _ => return Err(BbHeaderError::Crc),
        };
        let m1 = h[0];
        Ok(BbHeader {
            format: match m1 >> 6 {
                0 => StreamFormat::GenericPacketized,
                1 => StreamFormat::GenericContinuous,
                2 => StreamFormat::Reserved,
                _ => StreamFormat::Transport,
            },
            single_stream: m1 & 0x20 != 0,
            ccm: m1 & 0x10 != 0,
            issyi: m1 & 0x08 != 0,
            npd: m1 & 0x04 != 0,
            roll_off: match m1 & 3 {
                0 => Some(RollOff::R35),
                1 => Some(RollOff::R25),
                2 => Some(RollOff::R20),
                _ => None,
            },
            isi: h[1],
            upl: u16::from_be_bytes([h[2], h[3]]),
            dfl: u16::from_be_bytes([h[4], h[5]]),
            sync: h[6],
            syncd: u16::from_be_bytes([h[7], h[8]]),
            high_efficiency,
        })
    }

    /// The 10 header bytes, CRC included.
    pub fn to_bytes(&self) -> [u8; BBHEADER_LEN] {
        let ts_gs = match self.format {
            StreamFormat::GenericPacketized => 0,
            StreamFormat::GenericContinuous => 1,
            StreamFormat::Reserved => 2,
            StreamFormat::Transport => 3,
        };
        let ro = match self.roll_off {
            Some(RollOff::R35) => 0,
            Some(RollOff::R25) => 1,
            Some(RollOff::R20) => 2,
            _ => 3,
        };
        let m1 = (ts_gs << 6)
            | (self.single_stream as u8) << 5
            | (self.ccm as u8) << 4
            | (self.issyi as u8) << 3
            | (self.npd as u8) << 2
            | ro;
        let mut h = [0u8; BBHEADER_LEN];
        h[0] = m1;
        h[1] = self.isi;
        h[2..4].copy_from_slice(&self.upl.to_be_bytes());
        h[4..6].copy_from_slice(&self.dfl.to_be_bytes());
        h[6] = self.sync;
        h[7..9].copy_from_slice(&self.syncd.to_be_bytes());
        h[9] = crc8(&h[..9]) ^ self.high_efficiency as u8;
        h
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc8_check_value() {
        // The CRC-8/DVB-S2 catalogue check value.
        assert_eq!(crc8(b"123456789"), 0xBC);
    }

    #[test]
    fn bb_sequence_starts_as_figure_5_gives() {
        // Worked by hand from the register in Figure 5 (and matching
        // gr-dvbs2rx's descrambler): 0000 0011 1111 0110 …
        assert_eq!(
            &bb_sequence()[..8],
            &[0x03, 0xF6, 0x08, 0x34, 0x30, 0xB8, 0xA3, 0x93]
        );
    }

    #[test]
    fn scrambling_is_its_own_inverse() {
        let orig: Vec<u8> = (0..7274u32).map(|i| (i * 37 + 11) as u8).collect();
        let mut f = orig.clone();
        bb_scramble(&mut f);
        assert_ne!(f, orig);
        bb_scramble(&mut f);
        assert_eq!(f, orig);
    }

    #[test]
    fn header_round_trip_and_mode() {
        let h = BbHeader {
            format: StreamFormat::Transport,
            single_stream: true,
            ccm: false,
            issyi: false,
            npd: true,
            roll_off: Some(RollOff::R20),
            isi: 0,
            upl: 188 * 8,
            dfl: 58_112,
            sync: 0x47,
            syncd: 1234,
            high_efficiency: false,
        };
        let b = h.to_bytes();
        assert_eq!(b[0], 0b1110_0110);
        assert_eq!(BbHeader::parse(&b), Ok(h));
        let hem = BbHeader {
            high_efficiency: true,
            ..h
        };
        assert_eq!(BbHeader::parse(&hem.to_bytes()), Ok(hem));
        let mut bad = b;
        bad[4] ^= 0x10;
        assert_eq!(BbHeader::parse(&bad), Err(BbHeaderError::Crc));
    }
}
