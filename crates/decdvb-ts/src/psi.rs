//! What a transport stream carries: per-PID counts and continuity, and the
//! programme tables — PAT, PMT (ISO/IEC 13818-1 §2.4.4) and SDT
//! (EN 300 468 §5.2.3) — for the service names and stream types.

use std::collections::BTreeMap;

use decdvb_core::crc::crc32_mpeg2;

use crate::deframe::TS_LEN;

pub const PID_PAT: u16 = 0x0000;
pub const PID_SDT: u16 = 0x0011;
pub const PID_NULL: u16 = 0x1FFF;

/// A stream type (PMT) as a short name.
pub fn stream_type_name(t: u8) -> &'static str {
    match t {
        0x01 => "MPEG-1 video",
        0x02 => "MPEG-2 video",
        0x03 => "MPEG-1 audio",
        0x04 => "MPEG-2 audio",
        0x05 => "private sections",
        0x06 => "private PES (subtitles/teletext/AC-3)",
        0x0F => "AAC audio",
        0x10 => "MPEG-4 video",
        0x11 => "AAC (LATM) audio",
        0x15 => "metadata",
        0x1B => "H.264 video",
        0x24 => "HEVC video",
        0x81 => "AC-3 audio",
        0x87 => "E-AC-3 audio",
        _ => "other",
    }
}

/// One elementary stream of a programme.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EsInfo {
    pub pid: u16,
    pub stream_type: u8,
}

/// A programme: what the PAT, its PMT and the SDT say.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Programme {
    pub number: u16,
    pub pmt_pid: u16,
    pub pcr_pid: Option<u16>,
    pub streams: Vec<EsInfo>,
    pub provider: Option<String>,
    pub name: Option<String>,
}

/// Per-PID counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PidStats {
    pub packets: u64,
    /// Continuity counter jumps (packets lost, or a broken stream).
    pub cc_errors: u64,
    /// Packets with the transport error indicator set.
    pub errors: u64,
    /// Packets with scrambling control set: encrypted (conditional access).
    pub scrambled: u64,
}

/// A table being reassembled from TS packets.
#[derive(Default)]
struct Section {
    data: Vec<u8>,
}

/// Watches a transport stream: PID counts, continuity, and the PSI/SI.
#[derive(Default)]
pub struct TsAnalyser {
    pub pids: BTreeMap<u16, PidStats>,
    last_cc: BTreeMap<u16, u8>,
    sections: BTreeMap<u16, Section>,
    /// Programmes by number, from the PAT.
    pub programmes: BTreeMap<u16, Programme>,
    /// PIDs the PAT names as PMTs.
    pmt_pids: BTreeMap<u16, u16>,
    pub packets: u64,
    /// The transport stream id, from the PAT.
    pub ts_id: Option<u16>,
    pub bad_sections: u64,
}

impl TsAnalyser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Look at one packet.
    pub fn packet(&mut self, p: &[u8; TS_LEN]) {
        self.packets += 1;
        let tei = p[1] & 0x80 != 0;
        let pusi = p[1] & 0x40 != 0;
        let pid = u16::from_be_bytes([p[1] & 0x1F, p[2]]);
        let scrambling = p[3] >> 6;
        let afc = (p[3] >> 4) & 3;
        let cc = p[3] & 0x0F;
        let st = self.pids.entry(pid).or_default();
        st.packets += 1;
        if tei {
            st.errors += 1;
            return; // nothing in it can be trusted
        }
        if scrambling != 0 {
            st.scrambled += 1;
        }
        let has_payload = afc & 1 == 1;
        // Continuity: payload packets count up mod 16; a repeat is allowed
        // once, and the null PID is exempt.
        let discontinuity = afc & 2 != 0 && p[4] > 0 && p[5] & 0x80 != 0;
        if pid != PID_NULL && has_payload {
            if let Some(&last) = self.last_cc.get(&pid)
                && !discontinuity
                && cc != (last + 1) & 0x0F
                && cc != last
            {
                st.cc_errors += 1;
            }
            self.last_cc.insert(pid, cc);
        }
        if !has_payload || scrambling != 0 {
            return;
        }
        let is_psi = pid == PID_PAT || pid == PID_SDT || self.pmt_pids.contains_key(&pid);
        if !is_psi {
            return;
        }
        // Payload start, past any adaptation field.
        let mut at = 4;
        if afc & 2 != 0 {
            at += 1 + p[4] as usize;
        }
        if at >= TS_LEN {
            return;
        }
        let payload = &p[at..];
        if pusi {
            // pointer_field: bytes finishing the previous section first.
            let ptr = payload[0] as usize;
            if 1 + ptr > payload.len() {
                return;
            }
            if let Some(s) = self.sections.get_mut(&pid) {
                s.data.extend_from_slice(&payload[1..1 + ptr]);
                let done = std::mem::take(&mut s.data);
                self.section(pid, &done);
            }
            self.sections.insert(pid, Section::default());
            let s = self.sections.get_mut(&pid).unwrap();
            s.data.extend_from_slice(&payload[1 + ptr..]);
        } else if let Some(s) = self.sections.get_mut(&pid) {
            s.data.extend_from_slice(payload);
        }
        // A complete section may already be in hand.
        if let Some(s) = self.sections.get(&pid)
            && s.data.len() >= 3
        {
            let len = 3 + (u16::from_be_bytes([s.data[1] & 0x0F, s.data[2]]) as usize);
            if s.data.len() >= len {
                let done = s.data[..len].to_vec();
                self.sections.remove(&pid);
                self.section(pid, &done);
            }
        }
    }

    /// One complete section (table id, section length, … CRC-32).
    fn section(&mut self, pid: u16, s: &[u8]) {
        if s.len() < 3 || s[0] == 0xFF {
            return; // stuffing
        }
        let len = 3 + (u16::from_be_bytes([s[1] & 0x0F, s[2]]) as usize);
        if s.len() < len || len < 12 || crc32_mpeg2(&s[..len]) != 0 {
            self.bad_sections += 1;
            return;
        }
        let s = &s[..len];
        let body = &s[8..len - 4];
        match (pid, s[0]) {
            (PID_PAT, 0x00) => {
                self.ts_id = Some(u16::from_be_bytes([s[3], s[4]]));
                for e in body.as_chunks::<4>().0 {
                    let number = u16::from_be_bytes([e[0], e[1]]);
                    let pmt = u16::from_be_bytes([e[2] & 0x1F, e[3]]);
                    if number == 0 {
                        continue; // the network PID
                    }
                    self.pmt_pids.insert(pmt, number);
                    let p = self.programmes.entry(number).or_default();
                    p.number = number;
                    p.pmt_pid = pmt;
                }
            }
            (pid, 0x02) if self.pmt_pids.contains_key(&pid) => {
                let number = u16::from_be_bytes([s[3], s[4]]);
                if body.len() < 4 {
                    return;
                }
                let pcr = u16::from_be_bytes([body[0] & 0x1F, body[1]]);
                let info_len = (u16::from_be_bytes([body[2] & 0x0F, body[3]])) as usize;
                let mut at = 4 + info_len;
                let mut streams = Vec::new();
                while at + 5 <= body.len() {
                    let stream_type = body[at];
                    let es_pid = u16::from_be_bytes([body[at + 1] & 0x1F, body[at + 2]]);
                    let es_info = u16::from_be_bytes([body[at + 3] & 0x0F, body[at + 4]]) as usize;
                    streams.push(EsInfo {
                        pid: es_pid,
                        stream_type,
                    });
                    at += 5 + es_info;
                }
                let p = self.programmes.entry(number).or_default();
                p.number = number;
                p.pmt_pid = pid;
                p.pcr_pid = Some(pcr);
                p.streams = streams;
            }
            (PID_SDT, 0x42) => {
                // SDT actual: original_network_id (2), reserved (1), then services.
                let mut at = 3;
                while at + 5 <= body.len() {
                    let sid = u16::from_be_bytes([body[at], body[at + 1]]);
                    let loop_len = u16::from_be_bytes([body[at + 3] & 0x0F, body[at + 4]]) as usize;
                    let desc = &body[(at + 5).min(body.len())..(at + 5 + loop_len).min(body.len())];
                    let p = self.programmes.entry(sid).or_default();
                    p.number = sid;
                    let mut d = 0;
                    while d + 2 <= desc.len() {
                        let (tag, dlen) = (desc[d], desc[d + 1] as usize);
                        let v = &desc[(d + 2).min(desc.len())..(d + 2 + dlen).min(desc.len())];
                        if tag == 0x48 && v.len() >= 2 {
                            // service_descriptor: type, provider name, service name.
                            let pl = v[1] as usize;
                            if 2 + pl < v.len() {
                                p.provider = Some(dvb_text(&v[2..2 + pl]));
                                let nl = v[2 + pl] as usize;
                                let name = &v[(3 + pl).min(v.len())..(3 + pl + nl).min(v.len())];
                                p.name = Some(dvb_text(name));
                            }
                        }
                        d += 2 + dlen;
                    }
                    at += 5 + loop_len;
                }
            }
            _ => {}
        }
    }
}

/// DVB text (EN 300 468 Annex A) as a Rust string: a leading byte below 0x20
/// selects a character table — read UTF-8 (0x15) as such, anything else as
/// Latin-1 — and control codes are dropped.
pub fn dvb_text(b: &[u8]) -> String {
    let (utf8, body) = match b.first() {
        Some(0x15) => (true, &b[1..]),
        Some(0x10) if b.len() >= 3 => (false, &b[3..]),
        Some(&c) if c < 0x20 => (false, &b[1..]),
        _ => (false, b),
    };
    let s: String = if utf8 {
        String::from_utf8_lossy(body).into_owned()
    } else {
        body.iter().map(|&c| c as char).collect()
    };
    s.chars()
        .filter(|c| !c.is_control() && !('\u{80}'..='\u{9F}').contains(c))
        .collect::<String>()
        .trim()
        .to_string()
}

/// Build a PSI section with its CRC (for tests and test signals).
pub fn section(table_id: u8, ext: u16, body: &[u8]) -> Vec<u8> {
    let len = 5 + body.len() + 4;
    let mut s = vec![
        table_id,
        0xB0 | ((len >> 8) as u8 & 0x0F),
        len as u8,
        (ext >> 8) as u8,
        ext as u8,
        0xC1, // version 0, current
        0,
        0,
    ];
    s.extend_from_slice(body);
    let crc = crc32_mpeg2(&s);
    s.extend_from_slice(&crc.to_be_bytes());
    s
}

/// Packetize one section on `pid` (it must fit one packet here).
pub fn section_packet(pid: u16, cc: u8, sec: &[u8]) -> [u8; TS_LEN] {
    let mut p = [0xFFu8; TS_LEN];
    p[0] = 0x47;
    p[1] = 0x40 | (pid >> 8) as u8 & 0x1F;
    p[2] = pid as u8;
    p[3] = 0x10 | (cc & 0x0F);
    p[4] = 0; // pointer field
    p[5..5 + sec.len()].copy_from_slice(sec);
    p
}

/// The PAT, PMT and SDT for one test programme: `number` on PMT PID
/// `pmt_pid`, one elementary stream, and a service name.
pub fn test_tables(
    number: u16,
    pmt_pid: u16,
    es: EsInfo,
    provider: &str,
    name: &str,
) -> [Vec<u8>; 3] {
    let mut pat = Vec::new();
    pat.extend_from_slice(&number.to_be_bytes());
    pat.extend_from_slice(&(0xE000 | pmt_pid).to_be_bytes());
    let pat = section(0x00, 1, &pat);

    let mut pmt = Vec::new();
    pmt.extend_from_slice(&(0xE000 | es.pid).to_be_bytes()); // PCR PID
    pmt.extend_from_slice(&0xF000u16.to_be_bytes()); // no programme info
    pmt.push(es.stream_type);
    pmt.extend_from_slice(&(0xE000 | es.pid).to_be_bytes());
    pmt.extend_from_slice(&0xF000u16.to_be_bytes());
    let pmt = section(0x02, number, &pmt);

    let mut desc = vec![0x48, 0, 0x01, provider.len() as u8];
    desc.extend_from_slice(provider.as_bytes());
    desc.push(name.len() as u8);
    desc.extend_from_slice(name.as_bytes());
    desc[1] = (desc.len() - 2) as u8;
    let mut sdt = vec![0, 1, 0xFF]; // original network id, reserved
    sdt.extend_from_slice(&number.to_be_bytes());
    sdt.push(0xFC);
    sdt.extend_from_slice(&((0x8000 | desc.len()) as u16).to_be_bytes()); // running, loop length
    sdt.extend_from_slice(&desc);
    let sdt = section(0x42, 1, &sdt);
    [pat, pmt, sdt]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_pat_pmt_and_sdt() {
        let [pat, pmt, sdt] = test_tables(
            7,
            0x1000,
            EsInfo {
                pid: 0x100,
                stream_type: 0x1B,
            },
            "DecDVB",
            "Test card",
        );
        let mut a = TsAnalyser::new();
        a.packet(&section_packet(PID_PAT, 0, &pat));
        a.packet(&section_packet(0x1000, 0, &pmt));
        a.packet(&section_packet(PID_SDT, 0, &sdt));
        let p = &a.programmes[&7];
        assert_eq!(p.pmt_pid, 0x1000);
        assert_eq!(
            p.streams,
            vec![EsInfo {
                pid: 0x100,
                stream_type: 0x1B
            }]
        );
        assert_eq!(p.name.as_deref(), Some("Test card"));
        assert_eq!(p.provider.as_deref(), Some("DecDVB"));
        assert_eq!(a.bad_sections, 0);
        assert_eq!(stream_type_name(0x1B), "H.264 video");
    }

    #[test]
    fn counts_continuity_errors() {
        let mut a = TsAnalyser::new();
        let mut p = [0u8; TS_LEN];
        p[0] = 0x47;
        p[1] = 0x01;
        for cc in [0u8, 1, 2, 2, 3, 5, 6] {
            p[3] = 0x10 | cc;
            a.packet(&p);
        }
        // 2 → 2 is a permitted repeat; 3 → 5 skips one.
        assert_eq!(a.pids[&0x100].cc_errors, 1);
        assert_eq!(a.pids[&0x100].packets, 7);
    }

    #[test]
    fn dvb_text_reads_utf8_and_latin1() {
        assert_eq!(dvb_text(b"\x15Caf\xc3\xa9"), "Café");
        assert_eq!(dvb_text(b"\x05Caf\xe9"), "Café");
        assert_eq!(dvb_text(b"Plain \x86name\x87"), "Plain name");
    }
}
