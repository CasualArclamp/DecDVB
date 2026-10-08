//! The transmit FEC chain: BBFRAME → BCH → LDPC → FECFRAME
//! (EN 302 307-1 §5.3), and BBFRAME sources for test signals: TS packets
//! (TS mode) or IP over GSE (generic continuous mode).

use std::collections::HashMap;

use decdvb_core::{FecFrame, RollOff};
use decdvb_fec::{Bch, FecParams, LdpcCode};
use decdvb_frame::{BBHEADER_LEN, BbHeader, StreamFormat, bb_scramble, crc8};
use decdvb_gse::{GseEncapsulator, Label, Variant};

/// Where a framer's BBFRAMEs come from.
pub trait BbFrameSource: Send {
    /// The next BBFRAME of `bytes` bytes, header included and BB-scrambled.
    fn next_frame(&mut self, bytes: usize) -> Vec<u8>;
    /// The roll-off the BBHEADER announces.
    fn set_roll_off(&mut self, roll_off: RollOff);
    /// CCM (one MODCOD) or ACM/VCM, for MATYPE.
    fn set_ccm(&mut self, ccm: bool);
}

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
    /// Packets made so far, and the PSI/SI tables sent every 40 of them
    /// (PID, section), with each table PID's continuity counter.
    seq: u64,
    tables: Vec<(u16, Vec<u8>)>,
    table_cc: std::collections::BTreeMap<u16, u8>,
    /// The rest of a packet that did not fit the previous frame.
    carry: Vec<u8>,
    /// CRC-8 of the last packet's 187 useful bytes.
    prev_crc: u8,
    pub roll_off: RollOff,
    /// CCM (one MODCOD) or ACM/VCM, for MATYPE.
    pub ccm: bool,
    /// A multicast radio in MPE on TEST_MPE_PID, and its packets queued.
    radio: TestRadio,
    mpe_queue: std::collections::VecDeque<[u8; TS_LEN]>,
    mpe_cc: u8,
}

/// The PID carrying IP (the test radio) in MPE.
pub const TEST_MPE_PID: u16 = 0x0FA0;

/// The test stream's data PID and its programme's PMT PID.
pub const TEST_PID: u16 = 0x0100;
pub const TEST_PMT_PID: u16 = 0x1000;

impl TsBbFramer {
    pub fn new(seed: u64) -> Self {
        TsBbFramer {
            rng: seed | 1,
            cc: 0,
            seq: 0,
            // One programme, its data on TEST_PID as private data (it is
            // noise, not video), named in the SDT.
            tables: decdvb_ts::psi::test_tables(
                1,
                TEST_PMT_PID,
                decdvb_ts::EsInfo::new(TEST_PID, 0x06),
                "DecDVB",
                "DecDVB test signal",
            ),
            table_cc: Default::default(),
            carry: Vec::new(),
            prev_crc: 0,
            roll_off: RollOff::R35,
            ccm: true,
            radio: TestRadio::new([239, 255, 1, 2], "DecDVB test radio (MPE)"),
            mpe_queue: Default::default(),
            mpe_cc: 0,
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

    /// The next test packet, sync byte included: the tables (PAT, PMT,
    /// SDT, EIT, NIT, TDT) every 40 packets, else data.
    fn packet(&mut self) -> [u8; TS_LEN] {
        let n = self.seq;
        self.seq += 1;
        let k = (n % 40) as usize;
        if k < self.tables.len() {
            let (pid, ref sec) = self.tables[k];
            let cc = self.table_cc.entry(pid).or_default();
            let p = decdvb_ts::psi::section_packet(pid, *cc, sec);
            *cc = cc.wrapping_add(1) & 0x0F;
            return p;
        }
        // Every fourth packet: the radio, in MPE.
        if n.is_multiple_of(4) {
            if self.mpe_queue.is_empty() {
                let ip = self.radio.next_packet();
                let sec = decdvb_ts::mpe::mpe_section([0x01, 0x00, 0x5E, 0x7F, 0x01, 0x02], &ip);
                self.mpe_queue.extend(decdvb_ts::mpe::packetize(
                    TEST_MPE_PID,
                    &mut self.mpe_cc,
                    &sec,
                ));
            }
            return self.mpe_queue.pop_front().unwrap();
        }
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
        BbFrameSource::next_frame(self, bytes)
    }
}

impl BbFrameSource for TsBbFramer {
    fn set_roll_off(&mut self, roll_off: RollOff) {
        self.roll_off = roll_off;
    }

    fn set_ccm(&mut self, ccm: bool) {
        self.ccm = ccm;
    }

    fn next_frame(&mut self, bytes: usize) -> Vec<u8> {
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

/// Builds GS-mode BBFRAMEs carrying IP over GSE: a deterministic mix of
/// UDP flows between documentation addresses (RFC 5737), packets of 40 to
/// 9000 bytes so that some fragment across frames, in any GSE [`Variant`].
pub struct GseBbFramer {
    enc: GseEncapsulator,
    radio: TestRadio,
    rng: u64,
    seq: u32,
    pub roll_off: RollOff,
    pub ccm: bool,
}

impl GseBbFramer {
    pub fn new(seed: u64, variant: Variant) -> Self {
        GseBbFramer {
            enc: GseEncapsulator::new(variant),
            radio: TestRadio::new([239, 255, 1, 1], "DecDVB test radio"),
            rng: seed | 1,
            seq: 0,
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

    /// The `n`-th test packet: every third one the test radio, else flow
    /// `n % 5` with a size from a fixed cycle.
    fn packet(&mut self) -> Vec<u8> {
        let n = self.seq;
        self.seq += 1;
        if n.is_multiple_of(3) {
            return self.radio.next_packet();
        }
        let flow = (n % 5) as u8;
        let len = [64usize, 1400, 512, 9000, 40, 1200, 200][n as usize % 7];
        let mut payload = vec![0u8; len];
        for chunk in payload.chunks_mut(8) {
            let r = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&r[..chunk.len()]);
        }
        decdvb_ip::packet::udp_v4(
            [192, 0, 2, 10 + flow],
            [198, 51, 100, 20 + flow],
            40_000 + flow as u16,
            5004,
            &payload,
        )
    }
}

impl BbFrameSource for GseBbFramer {
    fn set_roll_off(&mut self, roll_off: RollOff) {
        self.roll_off = roll_off;
    }

    fn set_ccm(&mut self, ccm: bool) {
        self.ccm = ccm;
    }

    fn next_frame(&mut self, bytes: usize) -> Vec<u8> {
        let field = bytes - BBHEADER_LEN;
        // Keep the queue topped up so every frame is full.
        while self.enc.queued() < 4 {
            let p = self.packet();
            self.enc
                .push(0x0800, Label::Six([0x02, 0, 0, 0, 0, 0x01]), p);
        }
        let data = self.enc.fill(field);
        let header = BbHeader {
            format: StreamFormat::GenericContinuous,
            single_stream: true,
            ccm: self.ccm,
            issyi: false,
            npd: false,
            roll_off: Some(self.roll_off),
            isi: 0,
            upl: 0,
            dfl: (field * 8) as u16,
            sync: 0,
            syncd: 0,
            high_efficiency: false,
        };
        let mut frame = Vec::with_capacity(bytes);
        frame.extend_from_slice(&header.to_bytes());
        frame.extend_from_slice(&data);
        bb_scramble(&mut frame);
        frame
    }
}

/// A multicast "radio" for test signals: RTP MPEG audio (payload type 14)
/// of silent layer II frames, announced by SAP every 50 packets.
pub struct TestRadio {
    group: [u8; 4],
    name: &'static str,
    n: u64,
    seq: u16,
    ts: u32,
}

impl TestRadio {
    pub fn new(group: [u8; 4], name: &'static str) -> Self {
        TestRadio {
            group,
            name,
            n: 0,
            seq: 0,
            ts: 0,
        }
    }

    /// The next IP packet: a SAP announcement or an RTP packet.
    pub fn next_packet(&mut self) -> Vec<u8> {
        use decdvb_ip::mcast::{rtp_packet, sap_packet, silent_mp2_frame};
        use decdvb_ip::packet::udp_v4;
        let src = [192, 0, 2, 99];
        let n = self.n;
        self.n += 1;
        if n.is_multiple_of(50) {
            let g = self.group;
            let sdp = format!(
                "v=0\r\no=- 1 1 IN IP4 192.0.2.99\r\ns={}\r\nc=IN IP4 {}.{}.{}.{}/32\r\nt=0 0\r\n\
                 m=audio 5004 RTP/AVP 14\r\n",
                self.name, g[0], g[1], g[2], g[3]
            );
            return udp_v4(
                src,
                [224, 2, 127, 254],
                9875,
                9875,
                &sap_packet(src, 1, &sdp),
            );
        }
        let mut payload = vec![0u8; 4]; // RFC 2250: MBZ and fragment offset
        payload.extend_from_slice(&silent_mp2_frame());
        let rtp = rtp_packet(
            14,
            self.seq,
            self.ts,
            0x0DEC_DB00 | self.group[3] as u32,
            &payload,
        );
        self.seq = self.seq.wrapping_add(1);
        self.ts = self.ts.wrapping_add(2160); // 1152 samples at 48 kHz, 90 kHz clock
        udp_v4(src, self.group, 4000, 5004, &rtp)
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
