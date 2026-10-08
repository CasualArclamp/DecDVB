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

use decdvb_core::{FecFrame, Iq, s2_modcod};
use decdvb_fec::demap::{demap_llr, quantize};
use decdvb_fec::{Bch, BchError, Constellation, DecodeOutcome, FecParams, LdpcCode, LdpcDecoder};
use decdvb_frame::{BBHEADER_LEN, BbHeader, BbHeaderError, PlsInfo, StreamFormat, bb_scramble};
use decdvb_gse::{GseIp, IpPacket, Source, Variant, VariantReport};
use decdvb_ip::{Flow, IpStats, PcapWriter};
use decdvb_ts::{TS_LEN, TcpSink, TsAnalyser, TsDeframer, TsFile, TsReport, UdpSink};

use crate::demod::{PILOT_AFTER, PILOT_PERIOD, PlFrame};

/// LLR quantization: steps per LLR unit (the decoder is happy from 2 to 8).
const LLR_SCALE: f32 = 4.0;
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
    cst: Constellation,
    ldpc: LdpcDecoder,
    bch: Bch,
}

/// Decodes PLFRAMEs; holds a decoder per code met so far.
#[derive(Default)]
pub struct FecDecoder {
    codes: HashMap<(bool, u8), Option<Code>>,
    data: Vec<Iq>,
    llr: Vec<f32>,
    quantized: Vec<i8>,
    info: Vec<u8>,
}

impl FecDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decode one frame; `None` for a dummy frame or a MODCOD with no S2 code
    /// (short 9/10, or the S2X ones until M3).
    pub fn decode(&mut self, f: &PlFrame) -> Option<BbFrame> {
        if f.pls.dummy_frame {
            return None;
        }
        let FecDecoder {
            codes,
            data,
            llr,
            quantized,
            info,
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
        if data.len() * code.cst.bits() as usize != n {
            return None;
        }
        llr.resize(n, 0.0);
        quantized.resize(n, 0);
        info.resize(p.n_bch / 8, 0);

        demap_llr(data, &code.cst, p.rate, f.noise_var * inv * inv, llr);
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

fn build_code(pls: PlsInfo) -> Option<Code> {
    let size = if pls.short_fecframe {
        FecFrame::Short
    } else {
        FecFrame::Normal
    };
    let mc = s2_modcod(pls.modcod, size)?;
    let params = FecParams::new(size, mc.rate)?;
    Some(Code {
        params,
        cst: Constellation::for_modcod(mc.modulation, mc.rate)?,
        ldpc: LdpcDecoder::new(LdpcCode::new(params.ldpc_table())),
        bch: Bch::new(size, params.t, params.n_bch),
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
}

impl IpStage {
    fn new() -> Self {
        IpStage {
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

    /// Open or close the PCAP to match `o.record` (a new file per recording).
    fn follow(&mut self, o: &FecOutput) {
        self.gse.forced = o.gse_variant;
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
        self.packets.clear();
        self.gse.data_field(field, &mut self.packets);
        let now = SystemTime::now();
        for p in &self.packets {
            self.stats.add(&p.info);
            self.win.0 += p.data.len() as f64;
            if let Some(w) = &mut self.pcap
                && let Err(e) = w.write(now, &p.data)
            {
                self.error = Some(e.to_string());
                self.pcap = None;
            }
        }
    }

    /// Signal time passes (every frame, good or not).
    fn tick(&mut self, secs: f64) {
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
        if self.last_top.elapsed().as_millis() >= 500 || self.top.is_empty() {
            self.top = self.stats.top_flows(8);
            self.last_top = Instant::now();
        }
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
        for p in &self.packets {
            self.analyser.packet(p);
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
            .filter(|s| s.modcod != 0)
            .map(|s| {
                let size = if s.short_fecframe {
                    FecFrame::Short
                } else {
                    FecFrame::Normal
                };
                let mc = s2_modcod(s.modcod, size).unwrap();
                let p = FecParams::new(size, mc.rate).unwrap();
                let mut f = ts.next_frame(p.bbframe_bytes());
                bb_scramble(&mut f);
                f
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
