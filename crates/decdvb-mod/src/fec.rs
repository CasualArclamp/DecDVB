//! The transmit FEC chain: BBFRAME → BCH → LDPC → FECFRAME
//! (EN 302 307-1 §5.3), and a TS-mode BBFRAME source for test signals.

use std::collections::HashMap;

use decdvb_core::{FecFrame, RollOff};
use decdvb_fec::{Bch, FecParams, LdpcCode};
use decdvb_frame::{BBHEADER_LEN, BbHeader, StreamFormat, bb_scramble, crc8};

/// BCH and LDPC encoders, built on first use per code.
#[derive(Default)]
pub struct FecEncoder {
    codes: HashMap<(FecFrame, u16, u16), (Bch, LdpcCode)>,
}

impl FecEncoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Encode one (scrambled) BBFRAME of `p.k_bch / 8` bytes into a FECFRAME
    /// of `p.n_ldpc / 8` bytes: the frame, its BCH parity, its LDPC parity.
    pub fn encode(&mut self, p: FecParams, bbframe: &[u8]) -> Vec<u8> {
        assert_eq!(bbframe.len() * 8, p.k_bch);
        let (bch, ldpc) = self
            .codes
            .entry((p.frame, p.rate.num, p.rate.den))
            .or_insert_with(|| {
                (
                    Bch::new(p.frame, p.t, p.n_bch),
                    LdpcCode::new(p.ldpc_table()),
                )
            });
        let mut out = vec![0u8; p.n_ldpc / 8];
        out[..bbframe.len()].copy_from_slice(bbframe);
        bch.encode(&mut out[..p.n_bch / 8]);
        let (info, parity) = out.split_at_mut(p.n_bch / 8);
        ldpc.encode(info, parity);
        out
    }
}

/// MPEG-TS packet length and sync byte.
const TS_LEN: usize = 188;
const TS_SYNC: u8 = 0x47;

/// Builds TS-mode BBFRAMEs (§5.1) from a deterministic stream of 188-byte
/// test packets on one PID with a running continuity counter.
///
/// Mode adaptation as §5.1.4 has it: user packets back to back across frames,
/// each packet's sync byte replaced by the CRC-8 of the previous packet's 187
/// bytes, and SYNCD giving the bit offset of the first packet that starts in
/// the data field.
pub struct TsBbFramer {
    rng: u64,
    cc: u8,
    /// The rest of a packet that did not fit the previous frame.
    carry: Vec<u8>,
    /// CRC-8 of the last packet's 187 useful bytes.
    prev_crc: u8,
    pub roll_off: RollOff,
    /// CCM (one MODCOD) or ACM/VCM, for MATYPE.
    pub ccm: bool,
}

/// The test stream's PID.
pub const TEST_PID: u16 = 0x0100;

impl TsBbFramer {
    pub fn new(seed: u64) -> Self {
        TsBbFramer {
            rng: seed | 1,
            cc: 0,
            carry: Vec::new(),
            prev_crc: 0,
            roll_off: RollOff::R35,
            ccm: true,
        }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// The next test packet, sync byte included.
    fn packet(&mut self) -> [u8; TS_LEN] {
        let mut p = [0u8; TS_LEN];
        p[0] = TS_SYNC;
        p[1] = (TEST_PID >> 8) as u8 & 0x1F;
        p[2] = TEST_PID as u8;
        p[3] = 0x10 | (self.cc & 0x0F); // payload only
        self.cc = self.cc.wrapping_add(1);
        for chunk in p[4..].chunks_mut(8) {
            let r = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&r[..chunk.len()]);
        }
        p
    }

    /// The next BBFRAME of `bytes` bytes, header included and BB-scrambled.
    pub fn next_frame(&mut self, bytes: usize) -> Vec<u8> {
        let field = bytes - BBHEADER_LEN;
        let mut data = Vec::with_capacity(field);
        data.extend_from_slice(&self.carry[..self.carry.len().min(field)]);
        let carried = data.len();
        self.carry.drain(..carried);
        // SYNCD: where the first packet starting in this field begins.
        let syncd = (data.len() * 8) as u16;
        while data.len() < field {
            let mut p = self.packet();
            // Sync byte out, CRC-8 of the previous packet's useful part in.
            p[0] = self.prev_crc;
            self.prev_crc = crc8(&p[1..]);
            let room = field - data.len();
            if room >= TS_LEN {
                data.extend_from_slice(&p);
            } else {
                data.extend_from_slice(&p[..room]);
                self.carry.extend_from_slice(&p[room..]);
            }
        }
        let header = BbHeader {
            format: StreamFormat::Transport,
            single_stream: true,
            ccm: self.ccm,
            issyi: false,
            npd: false,
            roll_off: Some(self.roll_off),
            isi: 0,
            upl: (TS_LEN * 8) as u16,
            dfl: (field * 8) as u16,
            sync: TS_SYNC,
            syncd,
            high_efficiency: false,
        };
        let mut frame = Vec::with_capacity(bytes);
        frame.extend_from_slice(&header.to_bytes());
        frame.extend_from_slice(&data);
        bb_scramble(&mut frame);
        frame
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use decdvb_core::CodeRate;
    use decdvb_fec::LdpcCode;

    #[test]
    fn frames_carry_a_valid_header_and_whole_packets() {
        let p = FecParams::new(FecFrame::Short, CodeRate::new(1, 2)).unwrap();
        let mut f = TsBbFramer::new(3);
        let mut stream = Vec::new();
        for _ in 0..5 {
            let mut fr = f.next_frame(p.bbframe_bytes());
            bb_scramble(&mut fr);
            let h = BbHeader::parse(&fr).unwrap();
            assert_eq!(h.format, StreamFormat::Transport);
            assert_eq!(h.dfl as usize, p.k_bch - 80);
            // SYNCD lands on a packet boundary of the concatenated stream.
            assert_eq!((stream.len() + h.syncd as usize / 8) % TS_LEN, 0);
            stream.extend_from_slice(&fr[BBHEADER_LEN..]);
        }
        // Every packet after the first starts with the CRC of the one before.
        for w in stream.as_chunks::<TS_LEN>().0.windows(2) {
            assert_eq!(w[1][0], crc8(&w[0][1..]));
        }
    }

    #[test]
    fn encoded_frames_are_ldpc_codewords() {
        let mut enc = FecEncoder::new();
        let mut f = TsBbFramer::new(4);
        for p in decdvb_fec::params::all_s2().step_by(4) {
            let bb = f.next_frame(p.bbframe_bytes());
            let fec = enc.encode(p, &bb);
            assert_eq!(&fec[..bb.len()], &bb[..]);
            assert!(LdpcCode::new(p.ldpc_table()).is_codeword(&fec), "{p:?}");
            let bch = Bch::new(p.frame, p.t, p.n_bch);
            let mut cw = fec[..p.n_bch / 8].to_vec();
            assert_eq!(bch.decode(&mut cw), Ok(0));
        }
    }
}
