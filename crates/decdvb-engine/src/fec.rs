//! The FEC stage of a DVB-S2 VFO: demodulated PLFRAMEs in, BBFRAMEs out.
//!
//! pilots out → scale by the measured gain → max-log LLRs, de-interleaved →
//! LDPC (layered min-sum) → BCH → BB descrambling → BBHEADER (CRC-8).
//!
//! It runs on its own thread per VFO (`FecWorker`), fed through a bounded
//! queue: the demodulator must keep real time, and when decoding cannot,
//! frames are dropped and counted rather than stalling it. Good GS-mode
//! BBFRAMEs then go through GSE to IP (every GSE variant tried, the one
//! yielding valid IP kept — `decdvb_gse::GseIp`), into live statistics and,
//! while recording, a PCAP file. Good TS-mode BBFRAMEs become MPEG-TS
//! (`decdvb_ts`): analysed (programmes, PIDs, continuity) and sent to a
//! `.ts` file, UDP, and/or a TCP/HTTP server for VLC or PotPlayer.

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Instant, SystemTime};

use decdvb_core::Iq;
use decdvb_fec::demap::{Mapper, quantize};
use decdvb_fec::vlsnr::VlsnrCode;
use decdvb_fec::{Bch, BchError, DecodeOutcome, FecParams, LdpcCode, LdpcDecoder};
use decdvb_frame::pi2bpsk::pi2_soft;
use decdvb_frame::vlsnr::{self as vl, Slot};
use decdvb_frame::{BBHEADER_LEN, BbHeader, BbHeaderError, PlsInfo, StreamFormat, bb_scramble};
use decdvb_gse::{GseIp, IpPacket, Source, Variant, VariantReport};
use decdvb_ip::mcast::udp_payload;
use decdvb_ip::{
    AudioRelay, AudioStream, Flow, IpInfo, IpStats, McastScanner, PcapWriter, PlayTarget,
};
use decdvb_ts::{
    MpeExtractor, MpeStats, TS_LEN, TcpSink, TsAnalyser, TsDeframer, TsFile, TsReport, UdpSink,
};

use crate::demod::{PILOT_AFTER, PILOT_PERIOD, PlFrame};
use decdvb_audio::{AudioHandle, AudioPlayer, AudioRecorder, OutputKind};

/// LLR quantization: steps per LLR unit (the decoder is happy from 2 to 8).
const LLR_SCALE: f32 = 4.0;
/// Where in-app audio plays: the sound card, except under test.
const AUDIO_OUTPUT: OutputKind = if cfg!(test) {
    OutputKind::Null
} else {
    OutputKind::Device
};
/// LDPC iteration budget per frame.
const MAX_ITERATIONS: usize = 50;
/// Frames queued for the FEC thread before new ones are dropped.
const QUEUE: usize = 64;

/// One decoded BBFRAME.
#[derive(Debug, Clone)]
pub struct BbFrame {
    pub pls: PlsInfo,
    /// The descrambled BBFRAME: header, data field, padding (K_bch bits).
    pub bytes: Vec<u8>,
    pub header: Result<BbHeader, BbHeaderError>,
    pub ldpc: DecodeOutcome,
    /// Bits BCH corrected, or why it could not.
    pub bch: Result<usize, BchError>,
    pub es_n0_db: f32,
}

impl BbFrame {
    /// BCH accepted the codeword and the BBHEADER's CRC-8 checks.
    pub fn ok(&self) -> bool {
        self.bch.is_ok() && self.header.is_ok()
    }
}

struct Code {
    params: FecParams,
    mapper: Mapper,
    ldpc: LdpcDecoder,
    bch: Bch,
}

/// A VL-SNR code's decoders.
struct VlCode {
    code: &'static VlsnrCode,
    ldpc: LdpcDecoder,
    bch: Bch,
    /// QPSK demapping for the 2/9 code.
    qpsk: Option<Mapper>,
}

/// Decodes PLFRAMEs; holds a decoder per code met so far.
#[derive(Default)]
pub struct FecDecoder {
    codes: HashMap<(bool, u8), Option<Code>>,
    vl_codes: HashMap<u8, Option<VlCode>>,
    sent: Vec<f32>,
    data: Vec<Iq>,
    llr: Vec<f32>,
    quantized: Vec<i8>,
    info: Vec<u8>,
}

impl FecDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decode one frame; `None` for a dummy frame, a frame with no code to
    /// decode (S2 short 9/10, S2X VL-SNR and reserved codes).
    pub fn decode(&mut self, f: &PlFrame) -> Option<BbFrame> {
        if f.pls.dummy_frame {
            return None;
        }
        if let (Some(set), Some((k, _))) = (f.pls.vlsnr, f.vlsnr) {
            return self.decode_vlsnr(f, set, k);
        }
        let FecDecoder {
            codes,
            data,
            llr,
            quantized,
            info,
            ..
        } = self;
        let code = codes
            .entry((f.pls.short_fecframe, f.pls.modcod))
            .or_insert_with(|| build_code(f.pls))
            .as_mut()?;
        let p = code.params;

        // Data symbols only, on the constellation's unit-power scale.
        let inv = 1.0 / f.gain.max(1e-6);
        data.clear();
        data.extend(
            f.payload
                .iter()
                .enumerate()
                .filter(|(i, _)| !f.pls.has_pilots || i % PILOT_PERIOD < PILOT_AFTER)
                .map(|(_, &y)| y * inv),
        );
        let n = p.n_ldpc;
        if data.len() != code.mapper.symbols() {
            return None;
        }
        llr.resize(n, 0.0);
        quantized.resize(n, 0);
        info.resize(p.n_bch / 8, 0);

        code.mapper.demap_llr(data, f.noise_var * inv * inv, llr);
        quantize(llr, LLR_SCALE, quantized);
        let ldpc = code.ldpc.decode(quantized, info, MAX_ITERATIONS);

        let mut bytes = info.clone();
        let bch = code.bch.decode(&mut bytes);
        bytes.truncate(p.bbframe_bytes());
        bb_scramble(&mut bytes);
        let header = BbHeader::parse(&bytes);
        Some(BbFrame {
            pls: f.pls,
            bytes,
            header,
            ldpc,
            bch,
            es_n0_db: f.es_n0_db(),
        })
    }
}

impl FecDecoder {
    /// A VL-SNR frame (EN 302 307-2 §5.5.2.6): data symbols from the
    /// layout, LLRs (pi/2-BPSK, spreading combined; or QPSK), the shortened
    /// and punctured bits put back, then LDPC and BCH as usual.
    fn decode_vlsnr(&mut self, f: &PlFrame, set: u8, k: u8) -> Option<BbFrame> {
        let vc = self
            .vl_codes
            .entry(k)
            .or_insert_with(|| {
                let code = VlsnrCode::for_header(k)?;
                Some(VlCode {
                    code,
                    ldpc: LdpcDecoder::new(LdpcCode::new(code.table)),
                    bch: Bch::new(code.frame, 12, code.n_bch),
                    qpsk: code.qpsk.then(|| {
                        Mapper::s2(
                            decdvb_fec::Constellation::qpsk(),
                            decdvb_core::CodeRate::new(2, 9),
                            code.sent_bits(),
                        )
                    }),
                })
            })
            .as_mut()?;
        let code = vc.code;
        let inv = 1.0 / f.gain.max(1e-6);
        let noise = (f.noise_var * inv * inv).max(1e-6);
        let layout = vl::layout(set);
        self.data.clear();
        self.data.extend(
            f.payload
                .iter()
                .zip(layout)
                .filter(|(_, s)| **s == Slot::Data)
                .map(|(&y, _)| y * inv),
        );
        if self.data.len() != code.symbols() {
            return None;
        }
        self.sent.clear();
        match &vc.qpsk {
            Some(m) => {
                self.sent.resize(code.sent_bits(), 0.0);
                m.demap_llr(&self.data, noise, &mut self.sent);
            }
            None => {
                // Unit-amplitude 2-PAM: LLR = 4·y/N0; spread bits add.
                let reps = if code.spread { 2 } else { 1 };
                for pair in self.data.chunks(reps).enumerate() {
                    let (j, ys) = pair;
                    let llr: f32 = ys
                        .iter()
                        .enumerate()
                        .map(|(r, &y)| 4.0 * pi2_soft(j * reps + r, y) / noise)
                        .sum();
                    self.sent.push(llr);
                }
            }
        }
        let n = code.n_ldpc();
        self.llr.resize(n, 0.0);
        code.expand_llr(&self.sent, &mut self.llr);
        self.quantized.resize(n, 0);
        quantize(&self.llr, LLR_SCALE, &mut self.quantized);
        self.info.resize(code.k_ldpc() / 8, 0);
        let ldpc = vc
            .ldpc
            .decode(&self.quantized, &mut self.info, MAX_ITERATIONS);

        let at = code.xs / 8;
        let mut bytes = self.info[at..at + code.n_bch / 8].to_vec();
        let bch = vc.bch.decode(&mut bytes);
        bytes.truncate(code.bbframe_bytes());
        bb_scramble(&mut bytes);
        // A ragged K (the medium codes): the last byte's spare bits are
        // BCH parity, not BBFRAME.
        if !code.k_bch.is_multiple_of(8)
            && let Some(last) = bytes.last_mut()
        {
            *last &= 0xFF << (8 - code.k_bch % 8);
        }
        let header = BbHeader::parse(&bytes);
        Some(BbFrame {
            pls: f.pls,
            bytes,
            header,
            ldpc,
            bch,
            es_n0_db: f.es_n0_db(),
        })
    }
}

fn build_code(pls: PlsInfo) -> Option<Code> {
    let mc = pls.modcod()?;
    let params = FecParams::new(mc.frame, mc.rate)?;
    Some(Code {
        params,
        mapper: Mapper::for_modcod(&mc)?,
        ldpc: LdpcDecoder::new(LdpcCode::new(params.ldpc_table())),
        bch: Bch::new(mc.frame, params.t, params.n_bch),
    })
}

/// What a VFO's FEC has done, for display.
#[derive(Debug, Clone, Default)]
pub struct FecStats {
    /// Data frames decoded (dummy frames excluded).
    pub frames: u64,
    /// BCH and the BBHEADER CRC both passed.
    pub ok: u64,
    /// BCH could not correct (LDPC left too many errors).
    pub bch_failed: u64,
    /// BCH passed but the BBHEADER CRC did not.
    pub crc_failed: u64,
    /// LDPC ended with checks unsatisfied (BCH may still have rescued it).
    pub ldpc_unconverged: u64,
    /// Frames dropped because decoding fell behind.
    pub dropped: u64,
    /// LDPC iterations, summed over `frames`.
    pub iterations: u64,
    /// Bits BCH corrected, summed.
    pub bch_corrected: u64,
    /// Es/N0 of the latest frame, from its known symbols.
    pub es_n0_db: Option<f32>,
    /// The latest valid BBHEADER.
    pub last_header: Option<BbHeader>,
    /// Good frames per input stream (ISI; 0 for a single stream).
    pub streams: BTreeMap<u8, u64>,
    /// (good, total) per MODCOD index.
    pub per_modcod: BTreeMap<u8, (u64, u64)>,
    /// Useful bit rate (BBHEADER DFL) over the last couple of seconds of
    /// signal, bits per second.
    pub payload_bps: f64,
    /// Fraction of real time the FEC thread is busy.
    pub load: f32,
    /// Good TS-mode frames.
    pub ts_frames: u64,
    /// What GSE and IP have made of the GS-mode frames.
    pub gse: Option<GseView>,
    /// The MPEG-TS from the TS-mode frames (MPEG-TS decoders).
    pub ts: Option<TsView>,
}

/// The transport stream and its outputs, for display.
#[derive(Debug, Clone, Default)]
pub struct TsView {
    pub packets: u64,
    /// Packets whose CRC-8 failed (flagged with the transport error bit).
    pub crc_errors: u64,
    /// Continuity counter jumps, all PIDs.
    pub cc_errors: u64,
    pub nulls_reinserted: u64,
    pub resyncs: u64,
    /// The stream uses ISSY, which is not read yet.
    pub issy: bool,
    /// TS rate over the last couple of seconds of signal, bits per second.
    pub ts_bps: f64,
    /// Everything the analyser knows: services, PIDs, network, tables.
    pub report: TsReport,
    pub file: Option<(PathBuf, u64)>,
    pub file_active: bool,
    /// UDP target and datagrams sent.
    pub udp: Option<(SocketAddr, u64)>,
    /// TCP server address and the players connected.
    pub tcp: Option<(SocketAddr, Vec<SocketAddr>)>,
    /// The last output error (a file that will not open, a port in use…).
    pub error: Option<String>,
}

/// IP out of GSE, for display.
#[derive(Debug, Clone, Default)]
pub struct GseView {
    /// Where packets come from: the GSE variant in use, or the blind search.
    pub source: Option<Source>,
    /// Every variant's counts, for comparison.
    pub variants: Vec<VariantReport>,
    /// Non-IP PDUs by protocol type.
    pub other_protocols: BTreeMap<u16, u64>,
    pub packets: u64,
    pub bytes: u64,
    pub ipv4: u64,
    pub ipv6: u64,
    /// Packets by IP protocol number.
    pub protocols: BTreeMap<u8, u64>,
    /// The busiest flows by bytes.
    pub top: Vec<Flow>,
    pub flows: usize,
    /// IP bit rate over the last couple of seconds of signal.
    pub ip_bps: f64,
    /// The PCAP being written, or the last one, and its packet count.
    pub pcap: Option<(PathBuf, u64)>,
    pub pcap_active: bool,
    pub pcap_error: Option<String>,
    /// IP that came by MPE from a transport stream rather than GSE.
    pub mpe: Option<MpeStats>,
    /// Multicast audio streams found, by address.
    pub audio: Vec<AudioStream>,
    pub sap_packets: u64,
    /// The stream being played, and what to open in the player.
    pub audio_playing: Option<SocketAddr>,
    pub audio_target: Option<PlayTarget>,
    pub audio_error: Option<String>,
    /// The in-app player, while one is playing: status, level, pause.
    pub audio_app: Option<AudioHandle>,
    /// The stream being recorded, and the file (current or last) with
    /// its bytes so far.
    pub audio_recording: Option<SocketAddr>,
    pub audio_record_file: Option<(PathBuf, u64)>,
    pub audio_record_error: Option<String>,
    /// Packets passed to the player so far.
    pub audio_forwarded: u64,
}

/// Where and whether a VFO's FEC writes its output.
#[derive(Debug, Clone, PartialEq)]
pub struct FecOutput {
    /// Write IP packets to a PCAP file.
    pub record: bool,
    pub dir: PathBuf,
    /// For file names.
    pub name: String,
    pub carrier_hz: f64,
    /// Read GSE this way instead of detecting it.
    pub gse_variant: Option<Variant>,
    /// The VFO's decoder is the MPEG-TS one: run the TS stage.
    pub ts: bool,
    /// Write the MPEG-TS to a `.ts` file.
    pub ts_record: bool,
    /// Send the MPEG-TS by UDP here.
    pub ts_udp: Option<SocketAddr>,
    /// Serve the MPEG-TS over TCP/HTTP here.
    pub ts_tcp: Option<SocketAddr>,
    /// Play this multicast audio stream (group:port): in the app, or
    /// relayed to an external player when `audio_external`.
    pub audio_play: Option<SocketAddr>,
    pub audio_external: bool,
    /// Record this multicast audio stream to a file in `dir`.
    pub audio_record: Option<SocketAddr>,
}

/// Runs a [`FecDecoder`] on its own thread.
pub(crate) struct FecWorker {
    tx: Option<SyncSender<PlFrame>>,
    stats: Arc<Mutex<FecStats>>,
    output: Arc<Mutex<FecOutput>>,
    /// A frame was dropped since the thread last looked: streams that span
    /// frames (TS) must start over.
    gap: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl FecWorker {
    /// `symbol_rate` turns frame lengths into signal time for the rates.
    pub fn spawn(symbol_rate: f64, output: FecOutput) -> Self {
        let (tx, rx) = mpsc::sync_channel(QUEUE);
        let stats = Arc::new(Mutex::new(FecStats::default()));
        let output = Arc::new(Mutex::new(output));
        let gap = Arc::new(AtomicBool::new(false));
        let (s, o, g) = (stats.clone(), output.clone(), gap.clone());
        let join = std::thread::Builder::new()
            .name("decdvb-fec".into())
            .spawn(move || run(rx, s, o, g, symbol_rate))
            .expect("spawn FEC thread");
        FecWorker {
            tx: Some(tx),
            stats,
            output,
            gap,
            join: Some(join),
        }
    }

    /// Queue a frame; drop it (counted) if the thread is behind.
    pub fn offer(&self, f: PlFrame) {
        if let Some(tx) = &self.tx
            && let Err(TrySendError::Full(_)) = tx.try_send(f)
        {
            self.stats.lock().unwrap().dropped += 1;
            self.gap.store(true, Ordering::Relaxed);
        }
    }

    /// Change what is written; takes effect from the next frame.
    pub fn set_output(&self, o: FecOutput) {
        *self.output.lock().unwrap() = o;
    }

    pub fn stats(&self) -> FecStats {
        self.stats.lock().unwrap().clone()
    }
}

impl Drop for FecWorker {
    fn drop(&mut self) {
        // Closing the queue ends the thread after its current frame.
        self.tx = None;
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// The GSE → IP → PCAP end of the FEC thread.
struct IpStage {
    gse: GseIp,
    stats: IpStats,
    packets: Vec<IpPacket>,
    pcap: Option<PcapWriter>,
    path: Option<PathBuf>,
    error: Option<String>,
    /// IP rate window: bytes and signal seconds.
    win: (f64, f64),
    ip_bps: f64,
    last_top: Instant,
    top: Vec<Flow>,
    mcast: McastScanner,
    audio: Vec<AudioStream>,
    relay: Option<AudioRelay>,
    player: Option<AudioPlayer>,
    /// Why the asked-for stream is not playing.
    relay_error: Option<String>,
    audio_external: bool,
    recorder: Option<AudioRecorder>,
    record_want: Option<SocketAddr>,
    record_error: Option<String>,
    /// The last recording's file and size, once it is closed.
    record_last: Option<(PathBuf, u64)>,
    /// The stream asked for (it may not have been heard yet).
    audio_want: Option<SocketAddr>,
    mpe: Option<MpeStats>,
}

impl IpStage {
    fn new() -> Self {
        IpStage {
            mcast: McastScanner::new(),
            audio: Vec::new(),
            relay: None,
            player: None,
            relay_error: None,
            audio_external: false,
            recorder: None,
            record_want: None,
            record_error: None,
            record_last: None,
            audio_want: None,
            mpe: None,
            gse: GseIp::new(),
            stats: IpStats::default(),
            packets: Vec::new(),
            pcap: None,
            path: None,
            error: None,
            win: (0.0, 0.0),
            ip_bps: 0.0,
            last_top: Instant::now(),
            top: Vec::new(),
        }
    }

    /// Open or close the PCAP to match `o.record` (a new file per
    /// recording), and start or stop playing audio to match `o.audio_play`.
    fn follow(&mut self, o: &FecOutput) {
        self.gse.forced = o.gse_variant;
        if o.audio_play != self.audio_want || o.audio_external != self.audio_external {
            self.audio_want = o.audio_play;
            self.audio_external = o.audio_external;
            self.relay = None;
            self.player = None;
            self.relay_error = None;
        }
        // Start playing once the stream has been seen enough to tell RTP
        // from raw and to know its codec: started early, it would be taken
        // apart the wrong way for good.
        if let Some(want) = self.audio_want
            && self.relay.is_none()
            && self.player.is_none()
            && self.relay_error.is_none()
            && let Some(st) = self.heard(want)
        {
            if self.audio_external {
                let dir = std::env::temp_dir().join("DecDVB");
                match AudioRelay::start(&st, &dir) {
                    Ok(r) => self.relay = Some(r),
                    Err(e) => self.relay_error = Some(format!("{}: {e}", st.name())),
                }
            } else {
                match AudioPlayer::start(&st, AUDIO_OUTPUT) {
                    Ok(p) => self.player = Some(p),
                    Err(e) => self.relay_error = Some(format!("{}: {e}", st.name())),
                }
            }
        }
        if o.audio_record != self.record_want {
            self.record_want = o.audio_record;
            self.stop_recording();
            self.record_error = None;
        }
        if let Some(want) = self.record_want
            && self.recorder.is_none()
            && self.record_error.is_none()
            && let Some(st) = self.heard(want)
        {
            let stamp = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let name: String = st
                .name()
                .chars()
                .map(|c| {
                    if c.is_alphanumeric() || c == '-' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect();
            let stem =
                format!("decdvb-{name}-{}_{}-{stamp}", st.group, st.port).replace([':', '.'], "_");
            match AudioRecorder::start(&st, &o.dir, &stem) {
                Ok(r) => self.recorder = Some(r),
                Err(e) => self.record_error = Some(format!("{}: {e}", st.name())),
            }
        }
        match (o.record, self.pcap.is_some()) {
            (true, false) if self.error.is_none() => {
                let stamp = SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let path = o.dir.join(format!(
                    "decdvb-{}-{:+.0}Hz-{stamp}.pcap",
                    o.name.replace(' ', "_"),
                    o.carrier_hz
                ));
                let _ = std::fs::create_dir_all(&o.dir);
                match PcapWriter::create(&path) {
                    Ok(w) => self.pcap = Some(w),
                    Err(e) => self.error = Some(format!("{}: {e}", path.display())),
                }
                self.path = Some(path);
            }
            (false, true) => {
                if let Some(mut w) = self.pcap.take() {
                    let _ = w.flush();
                }
            }
            (false, false) => self.error = None,
            _ => {}
        }
    }

    /// One good GS-mode frame's data field.
    fn data_field(&mut self, field: &[u8]) {
        let mut packets = std::mem::take(&mut self.packets);
        packets.clear();
        self.gse.data_field(field, &mut packets);
        for p in &packets {
            self.ip(&p.data, &p.info);
        }
        self.packets = packets;
    }

    /// IP datagrams from MPE (a transport stream).
    fn mpe(&mut self, datagrams: &[(Vec<u8>, IpInfo)], stats: &MpeStats) {
        for (d, info) in datagrams {
            self.ip(d, info);
        }
        self.mpe = Some(stats.clone());
    }

    /// One IP packet, wherever it came from.
    fn ip(&mut self, data: &[u8], info: &IpInfo) {
        self.stats.add(info);
        self.win.0 += data.len() as f64;
        self.mcast.packet(data, info);
        if (self.audio_want.is_some() || self.record_want.is_some())
            && let Some((payload, _, dport)) = udp_payload(data, info)
        {
            let key = Some(SocketAddr::new(info.dst, dport));
            if key == self.audio_want {
                if let Some(r) = &mut self.relay {
                    r.packet(payload);
                }
                if let Some(p) = &mut self.player {
                    p.packet(payload);
                }
            }
            if key == self.record_want
                && let Some(r) = &mut self.recorder
            {
                r.packet(payload);
            }
        }
        if let Some(w) = &mut self.pcap
            && let Err(e) = w.write(SystemTime::now(), data)
        {
            self.error = Some(e.to_string());
            self.pcap = None;
        }
    }

    /// Signal time passes (every frame, good or not).
    fn tick(&mut self, secs: f64) {
        self.mcast.tick(secs);
        self.win.1 += secs;
        if self.win.1 >= 2.0 || (self.ip_bps == 0.0 && self.win.1 > 0.2) {
            self.ip_bps = self.win.0 * 8.0 / self.win.1;
            if self.win.1 >= 2.0 {
                self.win = (0.0, 0.0);
            }
        }
    }

    fn view(&mut self) -> GseView {
        // Ranking flows sorts the table: twice a second is plenty.
        // Re-ranking sorts the flow table: twice a second, or at once while
        // new flows are still appearing in a short list.
        let new_flows = self.top.len() < 8 && self.stats.flow_count() != self.top.len();
        if self.last_top.elapsed().as_millis() >= 500 || new_flows {
            self.top = self.stats.top_flows(8);
            self.last_top = Instant::now();
        }
        // Few flows, cheap to list: always current.
        self.audio = self.mcast.streams();
        GseView {
            source: self.gse.source(),
            variants: self.gse.reports(),
            other_protocols: self.gse.other_protocols.clone(),
            packets: self.stats.packets,
            bytes: self.stats.bytes,
            ipv4: self.stats.ipv4,
            ipv6: self.stats.ipv6,
            protocols: self.stats.protocols.clone(),
            top: self.top.clone(),
            flows: self.stats.flow_count(),
            ip_bps: self.ip_bps,
            pcap: self
                .path
                .clone()
                .map(|p| (p, self.pcap.as_ref().map_or(0, |w| w.packets()))),
            pcap_active: self.pcap.is_some(),
            pcap_error: self.error.clone(),
            mpe: self.mpe.clone(),
            audio: self.audio.clone(),
            sap_packets: self.mcast.sap_packets,
            audio_playing: self.audio_want,
            audio_target: self.relay.as_ref().map(|r| r.target.clone()),
            audio_error: self.relay_error.clone(),
            audio_forwarded: self
                .relay
                .as_ref()
                .map(|r| r.forwarded)
                .or(self.player.as_ref().map(|p| p.forwarded))
                .unwrap_or(0),
            audio_app: self.player.as_ref().map(|p| p.handle()),
            audio_recording: self.record_want,
            audio_record_file: match &self.recorder {
                Some(r) => r.path().map(|p| (p.to_path_buf(), r.bytes)),
                None => self.record_last.clone(),
            },
            audio_record_error: self
                .record_error
                .clone()
                .or_else(|| self.recorder.as_ref().and_then(|r| r.error.clone())),
        }
    }

    /// The stream at `want`, once it has been heard enough to know how it
    /// is carried.
    fn heard(&self, want: SocketAddr) -> Option<AudioStream> {
        self.mcast
            .streams()
            .into_iter()
            .find(|a| a.group == want.ip() && a.port == want.port() && a.packets >= 8)
    }

    /// Close the recording, keeping its name and size for display.
    fn stop_recording(&mut self) {
        if let Some(r) = self.recorder.take() {
            self.record_last = r.path().map(|p| (p.to_path_buf(), r.bytes));
        }
    }
}

impl Drop for IpStage {
    fn drop(&mut self) {
        if let Some(w) = &mut self.pcap {
            let _ = w.flush();
        }
    }
}

/// The MPEG-TS end of the FEC thread.
struct TsStage {
    deframer: TsDeframer,
    analyser: TsAnalyser,
    packets: Vec<[u8; TS_LEN]>,
    file: Option<TsFile>,
    file_path: Option<PathBuf>,
    udp: Option<UdpSink>,
    tcp: Option<TcpSink>,
    /// What the outputs were last set to, so they are rebuilt only on change.
    udp_want: Option<SocketAddr>,
    tcp_want: Option<SocketAddr>,
    error: Option<String>,
    win: (f64, f64),
    ts_bps: f64,
    issy: bool,
    /// The analyser's last report, rebuilt at most every 250 ms.
    report: TsReport,
    report_at: Option<Instant>,
    /// IP in MPE sections, and the datagrams from the last field.
    mpe: MpeExtractor,
    datagrams: Vec<(Vec<u8>, IpInfo)>,
}

impl TsStage {
    fn new() -> Self {
        TsStage {
            deframer: TsDeframer::new(),
            analyser: TsAnalyser::new(),
            packets: Vec::new(),
            file: None,
            file_path: None,
            udp: None,
            tcp: None,
            udp_want: None,
            tcp_want: None,
            error: None,
            win: (0.0, 0.0),
            ts_bps: 0.0,
            issy: false,
            report: TsReport::default(),
            report_at: None,
            mpe: MpeExtractor::new(),
            datagrams: Vec::new(),
        }
    }

    /// Open, close or move the outputs to match `o`.
    fn follow(&mut self, o: &FecOutput) {
        match (o.ts_record, self.file.is_some()) {
            (true, false) if self.file_path.is_none() || self.error.is_none() => {
                let stamp = SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let path = o.dir.join(format!(
                    "decdvb-{}-{:+.0}Hz-{stamp}.ts",
                    o.name.replace(' ', "_"),
                    o.carrier_hz
                ));
                let _ = std::fs::create_dir_all(&o.dir);
                match TsFile::create(&path) {
                    Ok(f) => self.file = Some(f),
                    Err(e) => self.error = Some(format!("{}: {e}", path.display())),
                }
                self.file_path = Some(path);
            }
            (false, true) => {
                if let Some(mut f) = self.file.take() {
                    let _ = f.flush();
                }
            }
            _ => {}
        }
        if o.ts_udp != self.udp_want {
            self.udp_want = o.ts_udp;
            self.udp = None;
            if let Some(a) = o.ts_udp {
                match UdpSink::new(a) {
                    Ok(u) => self.udp = Some(u),
                    Err(e) => self.error = Some(format!("UDP {a}: {e}")),
                }
            }
        }
        if o.ts_tcp != self.tcp_want {
            self.tcp_want = o.ts_tcp;
            self.tcp = None; // closes the old server and its clients
            if let Some(a) = o.ts_tcp {
                match TcpSink::bind(a) {
                    Ok(t) => self.tcp = Some(t),
                    Err(e) => self.error = Some(format!("TCP {a}: {e}")),
                }
            }
        }
    }

    fn data_field(&mut self, field: &[u8], h: &BbHeader) {
        self.issy |= h.issyi;
        self.packets.clear();
        self.deframer
            .data_field(field, h.syncd, h.npd, h.issyi, &mut self.packets);
        self.datagrams.clear();
        for p in &self.packets {
            self.analyser.packet(p);
            self.mpe.packet(p, &mut self.datagrams);
        }
        self.win.0 += (self.packets.len() * TS_LEN) as f64;
        if let Some(f) = &mut self.file
            && let Err(e) = f.write(&self.packets)
        {
            self.error = Some(e.to_string());
            self.file = None;
        }
        if let Some(u) = &mut self.udp {
            u.write(&self.packets);
        }
        if let Some(t) = &mut self.tcp {
            t.write(&self.packets);
        }
    }

    fn tick(&mut self, secs: f64) {
        self.analyser.tick(secs);
        self.win.1 += secs;
        if self.win.1 >= 2.0 || (self.ts_bps == 0.0 && self.win.1 > 0.2) {
            self.ts_bps = self.win.0 * 8.0 / self.win.1;
            if self.win.1 >= 2.0 {
                self.win = (0.0, 0.0);
            }
        }
    }

    fn view(&mut self) -> TsView {
        // Classifying every PID costs more than a frame is worth on a busy
        // multiplex: refresh the report four times a second.
        if self
            .report_at
            .is_none_or(|t| t.elapsed().as_millis() >= 250)
        {
            self.report = self.analyser.report();
            self.report_at = Some(Instant::now());
        }
        let d = &self.deframer.stats;
        let cc_errors = self.analyser.pids.values().map(|s| s.cc_errors).sum();
        TsView {
            packets: d.packets,
            crc_errors: d.crc_errors,
            cc_errors,
            nulls_reinserted: d.nulls_reinserted,
            resyncs: d.resyncs,
            issy: self.issy,
            ts_bps: self.ts_bps,
            report: self.report.clone(),
            file: self
                .file_path
                .clone()
                .map(|p| (p, self.file.as_ref().map_or(0, |f| f.packets))),
            file_active: self.file.is_some(),
            udp: self.udp.as_ref().map(|u| (u.target, u.datagrams)),
            tcp: self.tcp.as_ref().map(|t| (t.addr, t.clients())),
            error: self.error.clone(),
        }
    }
}

impl Drop for TsStage {
    fn drop(&mut self) {
        if let Some(f) = &mut self.file {
            let _ = f.flush();
        }
    }
}

fn run(
    rx: Receiver<PlFrame>,
    stats: Arc<Mutex<FecStats>>,
    output: Arc<Mutex<FecOutput>>,
    gap: Arc<AtomicBool>,
    symbol_rate: f64,
) {
    let mut dec = FecDecoder::new();
    let mut ip: Option<IpStage> = None;
    let mut ts: Option<TsStage> = None;
    // Payload rate window: DFL bits and signal seconds.
    let (mut win_bits, mut win_secs) = (0f64, 0f64);
    let mut busy = 0f64;
    while let Ok(f) = rx.recv() {
        let t0 = Instant::now();
        let secs = f.pls.plframe_len as f64 / symbol_rate;
        let out = dec.decode(&f);

        // A stream reassembled across frames breaks at any frame missing in
        // between: one the demodulator never saw (lock regained), one dropped
        // from the queue, or one that would not decode.
        let broken = f.after_gap
            || gap.swap(false, Ordering::Relaxed)
            || out.as_ref().is_some_and(|b| !b.ok());
        if broken && let Some(t) = &mut ts {
            t.deframer.discontinuity();
        }
        let wants_ts = output.lock().unwrap().ts;
        if let Some(b) = &out
            && let Ok(h) = &b.header
            && b.ok()
            && h.format == StreamFormat::Transport
            && wants_ts
        {
            let end = (BBHEADER_LEN + h.dfl as usize / 8).min(b.bytes.len());
            let stage = ts.get_or_insert_with(TsStage::new);
            stage.follow(&output.lock().unwrap());
            stage.data_field(&b.bytes[BBHEADER_LEN..end], h);
            // IP by MPE joins the IP stage (statistics, multicast audio).
            if !stage.datagrams.is_empty() || ip.is_some() {
                let ipst = ip.get_or_insert_with(IpStage::new);
                ipst.follow(&output.lock().unwrap());
                ipst.mpe(&stage.datagrams, &stage.mpe.stats);
            }
        } else if let Some(t) = &mut ts {
            // Keep Record/UDP/TCP responsive between TS frames.
            t.follow(&output.lock().unwrap());
        }
        if let Some(t) = &mut ts {
            t.tick(secs);
        }

        // GSE/IP on good generic-stream frames (UPL 0: continuous; GSE also
        // turns up flagged as packetized with UPL 0).
        let gs = out.as_ref().and_then(|b| match &b.header {
            Ok(h) if b.ok() && h.format != StreamFormat::Transport && h.upl == 0 => {
                let end = (BBHEADER_LEN + h.dfl as usize / 8).min(b.bytes.len());
                Some(&b.bytes[BBHEADER_LEN..end])
            }
            _ => None,
        });
        if let Some(field) = gs {
            let stage = ip.get_or_insert_with(IpStage::new);
            stage.follow(&output.lock().unwrap());
            stage.data_field(field);
        }
        if let Some(stage) = &mut ip {
            stage.tick(secs);
            if gs.is_none() {
                // Keep Record/Stop responsive between GS frames.
                stage.follow(&output.lock().unwrap());
            }
        }

        let used = t0.elapsed().as_secs_f64();
        busy = 0.95 * busy + 0.05 * (used / secs.max(1e-9));

        let mut s = stats.lock().unwrap();
        s.load = busy as f32;
        win_secs += secs;
        if let Some(b) = &out {
            s.frames += 1;
            s.iterations += b.ldpc.iterations as u64;
            s.ldpc_unconverged += !b.ldpc.converged as u64;
            s.es_n0_db = Some(b.es_n0_db);
            let entry = s.per_modcod.entry(b.pls.modcod).or_default();
            entry.1 += 1;
            match (&b.bch, &b.header) {
                (Err(_), _) => s.bch_failed += 1,
                (Ok(_), Err(_)) => s.crc_failed += 1,
                (Ok(c), Ok(h)) => {
                    s.ok += 1;
                    s.bch_corrected += *c as u64;
                    s.last_header = Some(*h);
                    *s.streams
                        .entry(if h.single_stream { 0 } else { h.isi })
                        .or_default() += 1;
                    s.per_modcod.entry(b.pls.modcod).or_default().0 += 1;
                    win_bits += h.dfl as f64;
                    if h.format == StreamFormat::Transport {
                        s.ts_frames += 1;
                    }
                }
            }
        }
        if win_secs >= 2.0 {
            s.payload_bps = win_bits / win_secs;
            (win_bits, win_secs) = (0.0, 0.0);
        } else if s.payload_bps == 0.0 && win_secs > 0.2 {
            // A first figure quickly, refined once the window fills.
            s.payload_bps = win_bits / win_secs;
        }
        if let Some(stage) = &mut ip {
            s.gse = Some(stage.view());
        }
        if let Some(t) = &mut ts {
            s.ts = Some(t.view());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demod::Demod;
    use decdvb_frame::StreamFormat;
    use decdvb_mod::{FrameSpec, PlFramer, Shaper, TsBbFramer};
    use std::f64::consts::TAU;

    /// A real DVB-S2 signal: `schedule` frames, shaped at 4 samples per
    /// symbol, offset by `cycles` per symbol, at `esn0_db`.
    fn signal(
        schedule: &[FrameSpec],
        n_sym: usize,
        cycles: f64,
        esn0_db: f64,
        seed: u64,
    ) -> Vec<Iq> {
        let mut framer = PlFramer::new(0, seed);
        let syms = framer.build_schedule(schedule, n_sym);
        let mut sh = Shaper::new(4, 0.25, 16);
        let mut x = Vec::new();
        sh.process(&syms, &mut x);
        let mut s = seed | 1;
        let mut uniform = move || {
            s ^= s >> 12;
            s ^= s << 25;
            s ^= s >> 27;
            (s.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
        };
        // The shaper's output has unit power per *sample*, so a symbol's
        // energy after the matched filter is 4× that: N0 per sample is
        // 4 · 10^(−Es/N0 / 10). (The receiver's estimate caught this when the
        // test first had it per symbol: it read 8.6 dB for "2.5".)
        let sigma = (4.0 * 10f64.powf(-esn0_db / 10.0)).sqrt() * std::f64::consts::FRAC_1_SQRT_2;
        x.iter()
            .enumerate()
            .map(|(n, &v)| {
                let ph = TAU * cycles * n as f64 / 4.0 + 0.9;
                let r = (-2.0 * uniform().max(1e-300).ln()).sqrt() * sigma;
                let t = TAU * uniform();
                v * Iq::new(ph.cos() as f32, ph.sin() as f32)
                    + Iq::new((r * t.cos()) as f32, (r * t.sin()) as f32)
            })
            .collect()
    }

    /// The BBFRAMEs a `PlFramer` with `seed` sends for `schedule`, in order,
    /// descrambled.
    fn expected(schedule: &[FrameSpec], frames: usize, seed: u64) -> Vec<Vec<u8>> {
        let mut ts = TsBbFramer::new(seed ^ 0x7E57);
        ts.ccm = schedule.iter().filter(|s| s.modcod != 0).all(|s| {
            (s.modcod, s.short_fecframe) == (schedule[0].modcod, schedule[0].short_fecframe)
        });
        schedule
            .iter()
            .cycle()
            .take(frames)
            // Frames with nothing to encode (dummy, reserved) take no BBFRAME.
            .filter_map(|s| {
                if let Some(k) = s.vlsnr {
                    // Whole bytes from the source; the receiver's ragged
                    // last byte is compared only that far.
                    let code = VlsnrCode::for_header(k)?;
                    let mut f = ts.next_frame(code.k_bch / 8);
                    bb_scramble(&mut f);
                    return Some(f);
                }
                let mc = s.info().modcod()?;
                let p = FecParams::new(mc.frame, mc.rate)?;
                let mut f = ts.next_frame(p.bbframe_bytes());
                bb_scramble(&mut f);
                Some(f)
            })
            .collect()
    }

    fn decode_all(x: &[Iq]) -> Vec<BbFrame> {
        let mut d = Demod::new(4.0, 1.0, 0.25, 0);
        let mut fec = FecDecoder::new();
        let mut frames = Vec::new();
        let mut out = Vec::new();
        for chunk in x.chunks(50_000) {
            frames.clear();
            d.process(chunk, &mut frames);
            out.extend(frames.iter().filter_map(|f| fec.decode(f)));
        }
        out
    }

    #[test]
    fn decodes_an_acm_carrier_to_the_bbframes_that_were_sent() {
        // QPSK 1/2, 8PSK 3/5 (the odd interleaver), short 16APSK 2/3, a dummy
        // and short 32APSK 3/4, 0.2 % off frequency, at 18 dB.
        let schedule = [
            FrameSpec::new(4, false, true),
            FrameSpec::new(12, false, true),
            FrameSpec::new(18, true, false),
            FrameSpec::new(0, false, false),
            FrameSpec::new(24, true, true),
        ];
        let x = signal(&schedule, 400_000, 0.002, 18.0, 21);
        let got = decode_all(&x);
        let want = expected(&schedule, 60, 21);
        assert!(got.len() >= 6, "only {} frames", got.len());
        // The first decoded frame is some frame of the sequence; every one
        // after must follow it exactly.
        let first = want
            .iter()
            .position(|w| *w == got[0].bytes)
            .expect("first frame not in the sent sequence");
        for (k, b) in got.iter().enumerate() {
            assert!(b.ok(), "frame {k}: {:?} {:?} {:?}", b.ldpc, b.bch, b.header);
            assert_eq!(
                b.bytes,
                want[first + k],
                "frame {k} (MODCOD {})",
                b.pls.modcod
            );
            let h = b.header.unwrap();
            assert_eq!(h.format, StreamFormat::Transport);
            assert!(!h.ccm, "an ACM schedule must say so");
        }
    }

    #[test]
    fn decodes_an_s2x_acm_carrier() {
        // One frame of each S2X constellation family, in ACM with S2 QPSK
        // 1/2 and a reserved code (kept lock through, nothing decoded), at
        // 30 dB for 256APSK's sake.
        let schedule = [
            FrameSpec::s2x(132, true), // QPSK 13/45
            FrameSpec::s2x(144, true), // 8PSK 25/36, interleaver "102"
            FrameSpec::s2x(138, true), // 2+4+2APSK 5/9-L
            FrameSpec::s2x(154, true), // 4+12APSK 26/45, "3201"
            FrameSpec::s2x(158, true), // 8+8APSK 3/5-L, points by table
            FrameSpec::s2x(178, true), // 4+8+4+16APSK 32/45
            FrameSpec::s2x(250, true), // reserved, 8-ary
            FrameSpec::s2x(186, true), // 4+12+20+28APSK 11/15
            FrameSpec::s2x(200, true), // 128APSK 3/4: 103 slots
            FrameSpec::s2x(214, true), // 256APSK 3/4
            FrameSpec::s2x(240, true), // short 4+12APSK 26/45, "2130"
            FrameSpec::new(4, false, true),
        ];
        let x = signal(&schedule, 600_000, 0.001, 30.0, 24);
        let got = decode_all(&x);
        let want = expected(&schedule, 80, 24);
        assert!(got.len() >= 12, "only {} frames", got.len());
        let first = want
            .iter()
            .position(|w| *w == got[0].bytes)
            .expect("first frame not in the sent sequence");
        let mut seen = std::collections::BTreeSet::new();
        for (k, b) in got.iter().enumerate() {
            let name = b.pls.modcod().map(|m| m.to_string()).unwrap_or_default();
            assert!(b.ok(), "frame {k} ({name}): {:?} {:?}", b.ldpc, b.bch);
            assert_eq!(b.bytes, want[first + k], "frame {k} ({name})");
            seen.insert(b.pls.modcod);
        }
        assert!(seen.len() >= 10, "MODCODs decoded: {seen:?}");
    }

    #[test]
    fn decodes_every_vlsnr_modcod() {
        // Set 1 (QPSK 2/9, pi/2-BPSK 1/5, 11/45, 1/3 medium, 1/5 and 11/45
        // SF2 short) and set 2 (pi/2-BPSK 1/5, 4/15, 1/3 short, and a dummy),
        // between S2 QPSK 1/2 frames, at 8 dB.
        let mut schedule = vec![FrameSpec::new(4, false, true)];
        for k in [0u8, 1, 2, 3, 4, 5, 9, 10, 11, 12] {
            schedule.push(FrameSpec::vlsnr(k));
        }
        let x = signal(&schedule, 600_000, 0.001, 8.0, 25);
        let got = decode_all(&x);
        let want = expected(&schedule, 40, 25);
        assert!(got.len() >= 10, "only {} frames", got.len());
        let first = want
            .iter()
            .position(|w| got[0].bytes.starts_with(w))
            .expect("first frame not in the sent sequence");
        let mut kinds = std::collections::BTreeSet::new();
        for (k, b) in got.iter().enumerate() {
            assert!(
                b.ok(),
                "frame {k} (PLS {}): {:?} {:?}",
                b.pls.plsc,
                b.ldpc,
                b.bch
            );
            let w = &want[first + k];
            assert_eq!(
                &b.bytes[..w.len()],
                &w[..],
                "frame {k} (PLS {})",
                b.pls.plsc
            );
            kinds.insert(b.bytes.len());
        }
        // Nine codes, nine BBFRAME sizes.
        assert!(kinds.len() >= 8, "sizes {kinds:?}");
    }

    /// How low VL-SNR goes with this receiver (run by hand:
    /// `cargo test -p decdvb-engine --release vlsnr_snr_sweep -- --ignored --nocapture`).
    #[test]
    #[ignore]
    fn vlsnr_snr_sweep() {
        for snr in [4.0, 2.0, 0.0, -2.0, -4.0] {
            let schedule = [
                FrameSpec::vlsnr(1),
                FrameSpec::vlsnr(4),
                FrameSpec::vlsnr(9),
            ];
            let x = signal(&schedule, 400_000, 0.001, snr, 26);
            let got = decode_all(&x);
            let ok = got.iter().filter(|b| b.ok()).count();
            println!("{snr:+.0} dB: {ok} of {} frames good", got.len());
        }
    }

    #[test]
    fn decodes_qpsk_near_its_threshold() {
        // QPSK 1/2 needs ~1 dB Es/N0 in theory. At 2 dB every frame must
        // come through — carrier and timing recovery, coherent header reads,
        // LLRs, LDPC, BCH — and the known-symbol Es/N0 must read true.
        // (Before the coherent header reads, frames failed from 6 dB down:
        // a QPSK 1/2 header read differentially as 3/5.)
        let schedule = [FrameSpec::new(4, false, true)];
        let x = signal(&schedule, 230_000, 0.001, 2.0, 22);
        let got = decode_all(&x);
        assert!(got.len() >= 5, "only {} frames", got.len());
        for b in &got {
            assert!(
                b.ok(),
                "{:?} {:?} {:?} at {:.1} dB",
                b.ldpc,
                b.bch,
                b.header,
                b.es_n0_db
            );
            assert!(
                (b.es_n0_db - 2.0).abs() < 0.5,
                "Es/N0 estimate {:.2}",
                b.es_n0_db
            );
        }
    }

    #[test]
    fn decodes_without_pilots_at_moderate_snr() {
        // No pilots: only the headers, 32 490 symbols apart, anchor the
        // phase; the decision-directed loop must hold in between. At 5 dB it
        // does; at 4 dB about one frame in seven is lost to a cycle slip —
        // which is what DVB-S2's pilots are for.
        let schedule = [FrameSpec::new(4, false, false)];
        let x = signal(&schedule, 230_000, 0.001, 5.0, 23);
        let got = decode_all(&x);
        assert!(got.len() >= 5, "only {} frames", got.len());
        let ok = got.iter().filter(|b| b.ok()).count();
        assert_eq!(ok, got.len(), "{ok} of {} frames", got.len());
    }
}
