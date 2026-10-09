//! Multicast audio: the radio feeds that satellite IP links carry as UDP
//! multicast — RTP or raw — found, named and classified.
//!
//! - **Flows**: every UDP datagram to a multicast group (224.0.0.0/4,
//!   ff00::/8) counts toward its (group, port) flow.
//! - **RTP** (RFC 3550) is recognised by version 2 headers whose SSRC holds
//!   and whose sequence numbers count up.
//! - **Codecs** from the payload's own sync words — AAC in ADTS, AAC in
//!   LOAS/LATM, MPEG audio (layer 1/2/3) — or from the RTP payload format:
//!   RFC 3640 AU headers, static payload type 14 (MPA).
//! - **SAP** (RFC 2974, port 9875) announcements carry SDP (RFC 8866) that
//!   name the streams and say exactly what they are; a stream so described
//!   is taken at its word.
//!
//! After the VK2SWL DVB-S/S2 multicast audio receiver, which does this from
//! MPE in a transport stream; here IP may come from GSE or MPE alike.

use std::collections::BTreeMap;
use std::net::IpAddr;

use crate::packet::IpInfo;

/// The SAP port.
pub const SAP_PORT: u16 = 9875;
/// RTP packets in a row that must look right before a flow counts as RTP.
const RTP_VOTES: u32 = 3;

/// What a stream carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Codec {
    /// AAC in ADTS frames.
    AacAdts,
    /// AAC (often HE-AAC) in LOAS/LATM.
    AacLatm,
    /// AAC in RFC 3640 (mpeg4-generic) AU headers.
    AacRfc3640,
    /// MPEG-1/2 audio, layer 1, 2 or 3.
    MpegAudio,
    /// Linear or G.711 PCM.
    Pcm,
    Opus,
    /// MPEG-TS in UDP.
    Ts,
    #[default]
    Unknown,
}

impl Codec {
    pub fn label(self) -> &'static str {
        match self {
            Codec::AacAdts => "AAC (ADTS)",
            Codec::AacLatm => "AAC (LATM/LOAS)",
            Codec::AacRfc3640 => "AAC (RFC 3640)",
            Codec::MpegAudio => "MPEG audio",
            Codec::Pcm => "PCM",
            Codec::Opus => "Opus",
            Codec::Ts => "MPEG-TS",
            Codec::Unknown => "unknown",
        }
    }

    pub fn is_audio(self) -> bool {
        !matches!(self, Codec::Ts | Codec::Unknown)
    }

    /// From an SDP encoding name ("MP4A-LATM", "mpeg4-generic", …).
    fn from_encoding(e: &str) -> Codec {
        let e = e.to_ascii_uppercase();
        match e.as_str() {
            "MP4A-LATM" => Codec::AacLatm,
            "MPEG4-GENERIC" => Codec::AacRfc3640,
            "MPA" | "MP3" => Codec::MpegAudio,
            "L16" | "L24" | "L8" | "PCMU" | "PCMA" => Codec::Pcm,
            "OPUS" => Codec::Opus,
            "MP2T" => Codec::Ts,
            _ => Codec::Unknown,
        }
    }

    /// From a static RTP payload type (RFC 3551).
    fn from_static_pt(pt: u8) -> Codec {
        match pt {
            14 => Codec::MpegAudio,
            0 | 8 | 10 | 11 => Codec::Pcm,
            33 => Codec::Ts,
            _ => Codec::Unknown,
        }
    }

    /// From the bytes a payload starts with.
    fn sniff(b: &[u8]) -> Codec {
        if b.len() < 4 {
            return Codec::Unknown;
        }
        if b[0] == 0xFF && b[1] & 0xF6 == 0xF0 {
            Codec::AacAdts // 12-bit sync, layer 00
        } else if b[0] == 0x56 && b[1] & 0xE0 == 0xE0 {
            Codec::AacLatm // 11-bit LOAS sync 0x2B7
        } else if b[0] == 0xFF && b[1] & 0xE0 == 0xE0 && (b[1] >> 1) & 3 != 0 && b[2] >> 4 != 0xF {
            Codec::MpegAudio // 11-bit sync, a layer, a valid bitrate
        } else if b[0] == 0x47 && b.len().is_multiple_of(188) {
            Codec::Ts
        } else if b[..2] == [0x00, 0x10] {
            // RFC 3640: 16 bits of AU headers (one 13-bit size + 3-bit index).
            let au = (u16::from_be_bytes([b[2], b[3]]) >> 3) as usize;
            if au + 4 == b.len() {
                Codec::AacRfc3640
            } else {
                Codec::Unknown
            }
        } else {
            Codec::Unknown
        }
    }
}

/// A session as SDP describes it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SdpInfo {
    pub name: Option<String>,
    pub info: Option<String>,
    pub group: Option<IpAddr>,
    pub port: Option<u16>,
    pub media: Option<String>,
    pub pt: Option<u8>,
    /// "MP4A-LATM/48000/2" and the like.
    pub encoding: Option<String>,
    /// The format parameters for that payload type ("profile-level-id=…;
    /// config=…"): where AAC carries its decoder configuration.
    pub fmtp: Option<String>,
    /// The SDP itself.
    pub raw: String,
}

impl SdpInfo {
    /// Parse the parts of an SDP that matter here (first media only).
    pub fn parse(text: &str) -> SdpInfo {
        let mut s = SdpInfo {
            raw: text.to_string(),
            ..Default::default()
        };
        // Lines end in CRLF or LF (RFC 4566 §5) — or a bare CR, as the
        // encoders on a live DVB-S2 radio multiplex send.
        for line in text.split(['\r', '\n']) {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            match k {
                "s" => s.name = Some(v.trim().to_string()).filter(|n| !n.is_empty() && n != "-"),
                "i" => s.info = Some(v.trim().to_string()),
                "c" => {
                    // c=IN IP4 239.1.2.3/32 — the media-level line wins.
                    if let Some(addr) = v.split_whitespace().nth(2) {
                        let addr = addr.split('/').next().unwrap_or(addr);
                        if let Ok(a) = addr.parse() {
                            s.group = Some(a);
                        }
                    }
                }
                "m" if s.media.is_none() => {
                    // m=audio 5004 RTP/AVP 96
                    let mut f = v.split_whitespace();
                    s.media = f.next().map(str::to_string);
                    s.port = f.next().and_then(|p| p.split('/').next()?.parse().ok());
                    s.pt = f.nth(1).and_then(|p| p.parse().ok());
                }
                "a" => {
                    if let Some(r) = v.strip_prefix("rtpmap:")
                        && let Some((pt, enc)) = r.split_once(' ')
                        && pt.parse::<u8>().ok() == s.pt
                    {
                        s.encoding = Some(enc.trim().to_string());
                    } else if let Some(r) = v.strip_prefix("fmtp:")
                        && let Some((pt, params)) = r.split_once(' ')
                        && pt.parse::<u8>().ok() == s.pt
                    {
                        s.fmtp = Some(params.trim().to_string());
                    }
                }
                _ => {}
            }
        }
        s
    }

    /// Every media section's group and port (RFC 4566 §5.7, §5.14: a
    /// connection line in a media section overrides the session's).
    pub fn endpoints(&self) -> Vec<(IpAddr, u16)> {
        let mut out = Vec::new();
        let mut session_c: Option<IpAddr> = None;
        let mut media: Option<(Option<IpAddr>, u16)> = None;
        let addr = |v: &str| -> Option<IpAddr> {
            v.split_whitespace().nth(2)?.split('/').next()?.parse().ok()
        };
        let mut flush = |m: &mut Option<(Option<IpAddr>, u16)>, sc: Option<IpAddr>| {
            if let Some((c, port)) = m.take()
                && let Some(g) = c.or(sc)
            {
                out.push((g, port));
            }
        };
        for line in self.raw.split(['\r', '\n']) {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            match k {
                "m" => {
                    flush(&mut media, session_c);
                    media = v
                        .split_whitespace()
                        .nth(1)
                        .and_then(|p| p.split('/').next()?.parse().ok())
                        .map(|port| (None, port));
                }
                "c" => match &mut media {
                    Some((c, _)) => *c = addr(v),
                    None => session_c = addr(v),
                },
                _ => {}
            }
        }
        flush(&mut media, session_c);
        out
    }

    pub fn codec(&self) -> Codec {
        match &self.encoding {
            Some(e) => Codec::from_encoding(e.split('/').next().unwrap_or("")),
            None => self.pt.map_or(Codec::Unknown, Codec::from_static_pt),
        }
    }
}

/// An SDP announcement in a UDP payload: SAP-framed (RFC 2974), or bare SDP
/// text as some encoders send to a port of their own.
pub fn announcement(p: &[u8]) -> Option<SdpInfo> {
    if p.starts_with(b"v=0") {
        let text = std::str::from_utf8(p).ok()?;
        let s = SdpInfo::parse(text);
        return (s.media.is_some() || s.name.is_some()).then_some(s);
    }
    sap_sdp(p)
}

/// The SDP in a SAP packet, if it carries one (announcements, not deletes;
/// encrypted and compressed ones are skipped).
pub fn sap_sdp(p: &[u8]) -> Option<SdpInfo> {
    if p.len() < 8 || p[0] >> 5 != 1 {
        return None; // SAP version 1
    }
    let ipv6 = p[0] & 0x10 != 0;
    let delete = p[0] & 0x04 != 0;
    let encrypted = p[0] & 0x02 != 0;
    let compressed = p[0] & 0x01 != 0;
    if delete || encrypted || compressed {
        return None;
    }
    let at = 4 + if ipv6 { 16 } else { 4 } + p[1] as usize * 4;
    let mut body = p.get(at..)?;
    // An optional payload type ("application/sdp"), NUL-terminated.
    if !body.starts_with(b"v=0")
        && let Some(nul) = body.iter().position(|&b| b == 0)
    {
        body = &body[nul + 1..];
    }
    let text = std::str::from_utf8(body).ok()?;
    text.starts_with("v=0").then(|| SdpInfo::parse(text))
}

/// The UDP payload of an IP packet, with its ports.
pub fn udp_payload<'a>(ip: &'a [u8], info: &IpInfo) -> Option<(&'a [u8], u16, u16)> {
    if info.protocol != crate::packet::PROTO_UDP {
        return None;
    }
    let ihl = if info.src.is_ipv4() {
        (ip[0] & 0x0F) as usize * 4
    } else {
        40
    };
    let udp = ip.get(ihl..info.len)?;
    if udp.len() < 8 {
        return None;
    }
    let (sport, dport) = (
        u16::from_be_bytes([udp[0], udp[1]]),
        u16::from_be_bytes([udp[2], udp[3]]),
    );
    Some((&udp[8..], sport, dport))
}

/// The RTP payload of a UDP payload already taken for RTP: past the CSRCs
/// and any header extension, padding removed.
pub fn rtp_payload(p: &[u8]) -> Option<&[u8]> {
    if p.len() < 12 || p[0] >> 6 != 2 {
        return None;
    }
    let mut at = 12 + 4 * (p[0] & 0x0F) as usize;
    if p[0] & 0x10 != 0 {
        let ext = p.get(at + 2..at + 4)?;
        at += 4 + 4 * u16::from_be_bytes([ext[0], ext[1]]) as usize;
    }
    let mut end = p.len();
    if p[0] & 0x20 != 0 {
        end = end.checked_sub(*p.last()? as usize)?;
    }
    p.get(at..end)
}

fn is_multicast(a: &IpAddr) -> bool {
    a.is_multicast()
}

/// One multicast flow.
#[derive(Debug, Clone, Default)]
struct Flow {
    src: Option<IpAddr>,
    packets: u64,
    bytes: u64,
    rtp_votes: u32,
    rtp: bool,
    ssrc: Option<u32>,
    seq: Option<u16>,
    pt: Option<u8>,
    sniffed: BTreeMap<u8, u32>,
    win_bytes: u64,
    rate_bps: f64,
}

/// A stream for display and playback.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioStream {
    pub group: IpAddr,
    pub port: u16,
    pub src: Option<IpAddr>,
    pub packets: u64,
    pub rate_bps: f64,
    pub rtp: bool,
    pub pt: Option<u8>,
    pub codec: Codec,
    /// What a SAP announcement says about it.
    pub sdp: Option<SdpInfo>,
}

impl AudioStream {
    /// The announced name, else the address.
    pub fn name(&self) -> String {
        self.sdp
            .as_ref()
            .and_then(|s| s.name.clone())
            .unwrap_or_else(|| format!("{}:{}", self.group, self.port))
    }
}

/// Watches IP packets for multicast audio.
#[derive(Default)]
pub struct McastScanner {
    flows: BTreeMap<(IpAddr, u16), Flow>,
    /// Sessions announced by SAP, keyed by their media group and port.
    pub announced: BTreeMap<(IpAddr, u16), SdpInfo>,
    pub sap_packets: u64,
    win_secs: f64,
}

/// Flows kept at most (a link carrying thousands is not radio).
const MAX_FLOWS: usize = 512;

/// Index of a codec in the sniff tally.
fn codec_index(c: Codec) -> u8 {
    c as u8
}

const CODECS: [Codec; 8] = [
    Codec::AacAdts,
    Codec::AacLatm,
    Codec::AacRfc3640,
    Codec::MpegAudio,
    Codec::Pcm,
    Codec::Opus,
    Codec::Ts,
    Codec::Unknown,
];

impl McastScanner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Look at one IP packet.
    pub fn packet(&mut self, ip: &[u8], info: &IpInfo) {
        if !is_multicast(&info.dst) {
            return;
        }
        let Some((payload, _, dport)) = udp_payload(ip, info) else {
            return;
        };
        // Announcements: SAP's port, or SDP sent anywhere (bare, or in a SAP
        // header on another port).
        let sdp = if dport == SAP_PORT {
            self.sap_packets += 1;
            sap_sdp(payload)
        } else if payload.starts_with(b"v=0") || (payload.len() > 8 && payload[0] >> 5 == 1) {
            announcement(payload)
        } else {
            None
        };
        if let Some(sdp) = sdp {
            if dport != SAP_PORT {
                self.sap_packets += 1;
            }
            for key in sdp.endpoints() {
                self.announced.insert(key, sdp.clone());
            }
            return;
        }
        if dport == SAP_PORT {
            return;
        }
        let key = (info.dst, dport);
        if !self.flows.contains_key(&key) && self.flows.len() >= MAX_FLOWS {
            return;
        }
        let f = self.flows.entry(key).or_default();
        f.src = Some(info.src);
        f.packets += 1;
        f.bytes += payload.len() as u64;
        f.win_bytes += payload.len() as u64;

        // RTP: version 2, a steady SSRC, sequence numbers counting up.
        let looks_rtp = payload.len() >= 12
            && payload[0] >> 6 == 2
            && !(72..=76).contains(&(payload[1] & 0x7F)); // not RTCP
        if looks_rtp {
            let ssrc = u32::from_be_bytes(payload[8..12].try_into().unwrap());
            let seq = u16::from_be_bytes([payload[2], payload[3]]);
            if f.ssrc == Some(ssrc) && f.seq.is_some_and(|s| s.wrapping_add(1) == seq) {
                f.rtp_votes += 1;
            }
            f.ssrc = Some(ssrc);
            f.seq = Some(seq);
            if f.rtp_votes >= RTP_VOTES {
                f.rtp = true;
                f.pt = Some(payload[1] & 0x7F);
            }
        } else {
            f.rtp_votes = f.rtp_votes.saturating_sub(1);
        }
        let body = if f.rtp {
            rtp_payload(payload).unwrap_or(&[])
        } else {
            payload
        };
        let mut c = Codec::sniff(body);
        if c == Codec::Unknown && f.rtp && f.pt == Some(14) && body.len() > 4 {
            c = Codec::sniff(&body[4..]); // RFC 2250's 4-byte MPA header
        }
        *f.sniffed.entry(codec_index(c)).or_default() += 1;
    }

    /// Signal time passed: update the flow rates once a second's worth is in.
    pub fn tick(&mut self, secs: f64) {
        self.win_secs += secs;
        if self.win_secs < 1.0 {
            return;
        }
        for f in self.flows.values_mut() {
            f.rate_bps = f.win_bytes as f64 * 8.0 / self.win_secs;
            f.win_bytes = 0;
        }
        self.win_secs = 0.0;
    }

    /// Whether `key` is (still) a known flow.
    pub fn has_flow(&self, group: IpAddr, port: u16) -> bool {
        self.flows.contains_key(&(group, port))
    }

    /// The announcement for a stream: its group and port, else the only one
    /// for its group, else the only one for its port (senders behind NAT or
    /// with a stale SDP get one of the two wrong).
    fn announcement_for(&self, group: IpAddr, port: u16) -> Option<SdpInfo> {
        if let Some(s) = self.announced.get(&(group, port)) {
            return Some(s.clone());
        }
        let only = |f: &dyn Fn(&(IpAddr, u16)) -> bool| {
            let mut it = self.announced.iter().filter(|(k, _)| f(k));
            match (it.next(), it.next()) {
                (Some((_, s)), None) => Some(s.clone()),
                _ => None,
            }
        };
        only(&|k| k.0 == group).or_else(|| only(&|k| k.1 == port))
    }

    /// Every station announced so far, by name (one per media address).
    pub fn stations(&self) -> Vec<(IpAddr, u16, SdpInfo)> {
        self.announced
            .iter()
            .map(|(&(g, p), s)| (g, p, s.clone()))
            .collect()
    }

    /// The multicast streams that carry audio (or are announced as audio),
    /// by group address and port.
    pub fn streams(&self) -> Vec<AudioStream> {
        let mut v: Vec<AudioStream> = self
            .flows
            .iter()
            .map(|(&(group, port), f)| {
                let sdp = self.announcement_for(group, port);
                let sniffed = f
                    .sniffed
                    .iter()
                    .filter(|(c, _)| **c != codec_index(Codec::Unknown))
                    .max_by_key(|(_, n)| **n)
                    .map(|(&c, _)| CODECS[c as usize])
                    .unwrap_or(Codec::Unknown);
                let codec = match (&sdp, f.rtp) {
                    (Some(s), _) if s.codec() != Codec::Unknown => s.codec(),
                    (_, true) if sniffed == Codec::Unknown => {
                        f.pt.map_or(Codec::Unknown, Codec::from_static_pt)
                    }
                    _ => sniffed,
                };
                AudioStream {
                    group,
                    port,
                    src: f.src,
                    packets: f.packets,
                    rate_bps: f.rate_bps,
                    rtp: f.rtp,
                    pt: f.pt,
                    codec,
                    sdp,
                }
            })
            .filter(|s| {
                s.codec.is_audio()
                    || s.sdp
                        .as_ref()
                        .is_some_and(|d| d.media.as_deref() == Some("audio"))
            })
            .collect();
        // By address, so the list stays put while rates move about.
        v.sort_by_key(|a| (a.group, a.port));
        v
    }
}

/// Build a SAP announcement of `sdp` (for tests and test signals).
pub fn sap_packet(origin: [u8; 4], msg_id: u16, sdp: &str) -> Vec<u8> {
    let mut p = vec![0x20, 0, (msg_id >> 8) as u8, msg_id as u8];
    p.extend_from_slice(&origin);
    p.extend_from_slice(b"application/sdp\0");
    p.extend_from_slice(sdp.as_bytes());
    p
}

/// Build an RTP packet (for tests and test signals).
pub fn rtp_packet(pt: u8, seq: u16, ts: u32, ssrc: u32, payload: &[u8]) -> Vec<u8> {
    let mut p = vec![0x80, pt & 0x7F];
    p.extend_from_slice(&seq.to_be_bytes());
    p.extend_from_slice(&ts.to_be_bytes());
    p.extend_from_slice(&ssrc.to_be_bytes());
    p.extend_from_slice(payload);
    p
}

/// A silent MPEG-1 layer II frame: 48 kHz, 128 kbit/s, stereo — the header,
/// then every allocation zero (no samples coded), so it decodes to silence.
pub fn silent_mp2_frame() -> Vec<u8> {
    let mut f = vec![0u8; 384];
    f[..4].copy_from_slice(&[0xFF, 0xFD, 0x84, 0x04]);
    f
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::{parse, udp_v4};

    fn feed(s: &mut McastScanner, p: &[u8]) {
        let info = parse(p).unwrap();
        s.packet(p, &info);
    }

    #[test]
    fn finds_an_announced_rtp_mp2_stream() {
        let mut s = McastScanner::new();
        let sdp = "v=0\r\no=- 1 1 IN IP4 10.0.0.1\r\ns=Test Radio\r\nc=IN IP4 239.255.1.1/32\r\n\
                   t=0 0\r\nm=audio 5004 RTP/AVP 14\r\n";
        feed(
            &mut s,
            &udp_v4(
                [10, 0, 0, 1],
                [224, 2, 127, 254],
                9875,
                9875,
                &sap_packet([10, 0, 0, 1], 7, sdp),
            ),
        );
        for k in 0..10u16 {
            let mut payload = vec![0u8; 4]; // RFC 2250 header
            payload.extend_from_slice(&silent_mp2_frame());
            let rtp = rtp_packet(14, k, k as u32 * 2160, 0xABCD, &payload);
            feed(
                &mut s,
                &udp_v4([10, 0, 0, 1], [239, 255, 1, 1], 4000, 5004, &rtp),
            );
        }
        s.tick(1.0);
        let v = s.streams();
        assert_eq!(v.len(), 1);
        let a = &v[0];
        assert_eq!(a.name(), "Test Radio");
        assert!(a.rtp);
        assert_eq!(a.pt, Some(14));
        assert_eq!(a.codec, Codec::MpegAudio);
        assert!(a.rate_bps > 0.0);
    }

    #[test]
    fn sniffs_raw_adts_and_latm_without_announcements() {
        let mut s = McastScanner::new();
        let adts = [0xFF, 0xF1, 0x50, 0x80, 0x02, 0x1F, 0xFC, 0, 0, 0];
        let loas = [0x56, 0xE0, 0x20, 0, 0, 0, 0, 0];
        for _ in 0..5 {
            feed(
                &mut s,
                &udp_v4([1, 1, 1, 1], [239, 1, 1, 1], 1, 1234, &adts),
            );
            feed(
                &mut s,
                &udp_v4([1, 1, 1, 1], [239, 1, 1, 2], 1, 1234, &loas),
            );
            // Unicast and non-audio multicast are ignored.
            feed(&mut s, &udp_v4([1, 1, 1, 1], [10, 1, 1, 2], 1, 1234, &adts));
            feed(
                &mut s,
                &udp_v4([1, 1, 1, 1], [239, 1, 1, 3], 1, 1234, b"hello world!"),
            );
        }
        let v = s.streams();
        assert_eq!(v.len(), 2);
        assert!(v.iter().any(|a| a.codec == Codec::AacAdts && !a.rtp));
        assert!(v.iter().any(|a| a.codec == Codec::AacLatm));
    }

    #[test]
    fn bare_sdp_on_any_port_names_its_streams() {
        // Bare SDP (no SAP header) to a port of its own, two media sections,
        // the second with its own group; the audio flows then carry names.
        let mut s = McastScanner::new();
        let sdp = "v=0\r\no=- 4 13 IN IP4 192.168.1.14\r\ns=Classic_Hits\r\ni=Classic_Hits\r\n\
                   c=IN IP4 230.0.0.3/16\r\nt=0 0\r\nm=audio 10001 RTP/AVP 14\r\n\
                   m=audio 10005 RTP/AVP 14\r\nc=IN IP4 230.0.0.5/16\r\n";
        feed(
            &mut s,
            &udp_v4([10, 1, 1, 1], [230, 0, 0, 1], 5000, 5555, sdp.as_bytes()),
        );
        for (g, port) in [([230, 0, 0, 3], 10001u16), ([230, 0, 0, 5], 10005)] {
            for k in 0..10u16 {
                let mut payload = vec![0u8; 4];
                payload.extend_from_slice(&silent_mp2_frame());
                let rtp = rtp_packet(14, k, k as u32 * 2160, 0x1234, &payload);
                feed(&mut s, &udp_v4([10, 1, 1, 1], g, 4000, port, &rtp));
            }
        }
        let v = s.streams();
        assert_eq!(v.len(), 2, "{v:?}");
        assert!(v.iter().all(|a| a.name() == "Classic_Hits"), "{v:?}");
        assert_eq!(s.stations().len(), 2);
    }

    #[test]
    fn sap_with_bare_cr_lines_names_the_station() {
        // As sent on a live DVB-S2 radio multiplex: SAP to 224.2.127.254,
        // SDP lines ended by a bare CR.
        let mut s = McastScanner::new();
        let sdp = "v=0\ro=- 1 47 IN IP4 192.168.1.11\rs=Newstalk_ZB\ri=Newstalk_ZB\r                   a=X-PID:1001\rm=audio 10001 RTP/AVP 14\rc=IN IP4 230.0.0.1\ra=bitrate:0\r";
        feed(
            &mut s,
            &udp_v4(
                [192, 168, 1, 11],
                [224, 2, 127, 254],
                9875,
                9875,
                &sap_packet([192, 168, 1, 11], 47, sdp),
            ),
        );
        for k in 0..10u16 {
            let mut payload = vec![0u8; 4];
            payload.extend_from_slice(&silent_mp2_frame());
            let rtp = rtp_packet(14, k, k as u32 * 2160, 0x99, &payload);
            feed(
                &mut s,
                &udp_v4([10, 152, 26, 11], [230, 0, 0, 1], 4000, 10001, &rtp),
            );
        }
        let v = s.streams();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].name(), "Newstalk_ZB");
        assert_eq!(v[0].sdp.as_ref().unwrap().pt, Some(14));
    }

    #[test]
    fn sdp_parses_the_media_line_and_rtpmap() {
        let s = SdpInfo::parse(
            "v=0\ns=Radio\nc=IN IP4 239.0.0.9/16\nm=audio 6000 RTP/AVP 96\na=rtpmap:96 MP4A-LATM/48000/2\n",
        );
        assert_eq!(s.port, Some(6000));
        assert_eq!(s.pt, Some(96));
        assert_eq!(s.group, Some("239.0.0.9".parse().unwrap()));
        assert_eq!(s.codec(), Codec::AacLatm);
    }
}
