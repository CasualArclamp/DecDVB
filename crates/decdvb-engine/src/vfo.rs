//! VFOs: a centre, a bandwidth and a decoder, each on its own worker thread.
//!
//! The wideband front end hands every VFO the same blocks (shared through an
//! `Arc`, so fanning out costs one pointer per VFO, not a copy). A worker that
//! falls behind has blocks dropped and counted rather than stalling the front
//! end — the waterfall and the other VFOs keep running in real time.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

use decdvb_core::{Iq, SampleFormat};
use decdvb_dsp::Ddc;
use decdvb_io::IqFileWriter;

use crate::demod::{Demod, LockState};
use crate::identify::{Identification, Verdict, identify_in};
use crate::spectrum::Spectrum;

pub type VfoId = u32;

/// What a VFO does with its signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DecoderKind {
    /// "What is this?" — blind symbol rate, roll-off, constellation, standard.
    Identify,
    /// DVB-S2/S2X → GSE → IP (PCAP).
    Dvbs2Ip,
    /// DVB-S2/S2X → MPEG-TS.
    Dvbs2Ts,
    /// Record the VFO's narrowband IQ.
    IqRecord,
    /// Zoomed spectrum and level only.
    Spectrum,
}

impl DecoderKind {
    pub const ALL: [DecoderKind; 5] = [
        DecoderKind::Identify,
        DecoderKind::Dvbs2Ip,
        DecoderKind::Dvbs2Ts,
        DecoderKind::IqRecord,
        DecoderKind::Spectrum,
    ];

    pub fn label(self) -> &'static str {
        match self {
            DecoderKind::Identify => "Identify (what is this?)",
            DecoderKind::Dvbs2Ip => "DVB-S2/S2X → GSE/IP (PCAP)",
            DecoderKind::Dvbs2Ts => "DVB-S2/S2X → MPEG-TS",
            DecoderKind::IqRecord => "IQ recorder",
            DecoderKind::Spectrum => "Spectrum only",
        }
    }

    pub fn short(self) -> &'static str {
        match self {
            DecoderKind::Identify => "ID",
            DecoderKind::Dvbs2Ip => "S2→IP",
            DecoderKind::Dvbs2Ts => "S2→TS",
            DecoderKind::IqRecord => "REC",
            DecoderKind::Spectrum => "SPEC",
        }
    }
}

/// A VFO's user-facing settings.
#[derive(Debug, Clone, PartialEq)]
pub struct VfoSettings {
    pub name: String,
    /// Centre relative to the wideband centre, Hz.
    pub offset_hz: f64,
    pub bandwidth_hz: f64,
    pub decoder: DecoderKind,
    pub enabled: bool,
    /// Known symbol rate; `None` finds it blind.
    pub symbol_rate: Option<f64>,
    /// PL scrambling gold-code index.
    pub gold_code: u32,
    /// Where the IQ recorder writes.
    pub record_dir: PathBuf,
}

impl VfoSettings {
    pub fn new(
        name: impl Into<String>,
        offset_hz: f64,
        bandwidth_hz: f64,
        decoder: DecoderKind,
    ) -> Self {
        VfoSettings {
            name: name.into(),
            offset_hz,
            bandwidth_hz,
            decoder,
            enabled: true,
            symbol_rate: None,
            gold_code: 0,
            record_dir: std::env::temp_dir(),
        }
    }
}

/// What a VFO worker publishes for the GUI. Cloned on read; kept small.
#[derive(Debug, Clone, Default)]
pub struct VfoStatus {
    pub out_rate: f64,
    pub decimation: usize,
    /// Fraction of real time the worker spends processing (1.0 = can't keep up).
    pub load: f32,
    pub dropped: u64,
    pub level_db: f32,
    /// The VFO's own spectrum, dB, FFT-shifted.
    pub spectrum_db: Vec<f32>,
    /// Points for the constellation view.
    pub scatter: Vec<Iq>,
    pub identification: Option<Identification>,
    /// 0..1 while gathering signal for Identify or DVB-S2 acquisition.
    pub progress: f32,
    /// Identify's last answer came from too short a listen to rule DVB-S2
    /// out at the symbol rate it found; a longer listen is under way.
    pub provisional: bool,
    pub lock: Option<LockState>,
    pub frames: u64,
    pub lock_losses: u64,
    /// MODCOD index → frames, DVB-S2 decoders.
    pub modcods: BTreeMap<u8, u64>,
    pub last_modcod: Option<u8>,
    pub symbol_rate: Option<f64>,
    pub message: String,
    pub recording: Option<(PathBuf, u64)>,
}

/// Messages on a worker's queue. Settings do not travel here: the queue is
/// bounded (that is how a slow worker sheds blocks), so a blocking send from
/// the GUI could freeze it behind a busy worker. Settings go in a mailbox slot
/// instead, and a non-blocking `Wake` tells the worker to look.
pub(crate) enum VfoMsg {
    Block(Arc<Vec<Iq>>),
    Wake,
}

/// The engine's handle on a running VFO.
pub(crate) struct VfoHandle {
    tx: SyncSender<VfoMsg>,
    pub status: Arc<Mutex<VfoStatus>>,
    pub settings: VfoSettings,
    dropped: Arc<AtomicU64>,
    /// Latest settings not yet taken by the worker.
    mailbox: Arc<Mutex<Option<VfoSettings>>>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl VfoHandle {
    /// Queue a block; drop it (and count) if the worker is behind.
    pub fn offer(&self, block: &Arc<Vec<Iq>>) {
        if !self.settings.enabled {
            return;
        }
        if let Err(TrySendError::Full(_)) = self.tx.try_send(VfoMsg::Block(Arc::clone(block))) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Hand the worker new settings without ever blocking the caller.
    pub fn post_settings(&mut self, settings: VfoSettings) {
        self.settings = settings.clone();
        *self.mailbox.lock().unwrap() = Some(settings);
        // If the queue is full the worker is busy and will see the mailbox
        // with its next block anyway.
        let _ = self.tx.try_send(VfoMsg::Wake);
    }

    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.tx.try_send(VfoMsg::Wake);
        // Dropping the sender also wakes a worker blocked on an empty queue.
        drop(self.tx);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// Blocks a worker may have queued before new ones are dropped.
const QUEUE_DEPTH: usize = 8;

pub(crate) fn spawn(in_rate: f64, settings: VfoSettings) -> VfoHandle {
    let (tx, rx) = mpsc::sync_channel(QUEUE_DEPTH);
    let status = Arc::new(Mutex::new(VfoStatus::default()));
    let dropped = Arc::new(AtomicU64::new(0));
    let mailbox = Arc::new(Mutex::new(None));
    let stop = Arc::new(AtomicBool::new(false));
    let join = {
        let status = Arc::clone(&status);
        let dropped = Arc::clone(&dropped);
        let mailbox = Arc::clone(&mailbox);
        let stop = Arc::clone(&stop);
        let settings = settings.clone();
        std::thread::Builder::new()
            .name(format!("vfo-{}", settings.name))
            .spawn(move || Worker::new(in_rate, settings, status, dropped).run(rx, mailbox, stop))
            .expect("spawning a VFO thread")
    };
    VfoHandle {
        tx,
        status,
        settings,
        dropped,
        mailbox,
        stop,
        join: Some(join),
    }
}

/// How much baseband to gather before identifying: enough for several normal
/// QPSK frames at any sensible samples-per-symbol, but bounded in time.
fn gather_target(out_rate: f64) -> usize {
    ((out_rate * 0.6) as usize).clamp(300_000, 2_000_000)
}

/// A first look for Identify: ~0.6 s, but never more than 2 s of signal, so a
/// narrow VFO answers quickly. Whether that was long enough to judge DVB-S2 is
/// decided afterwards, from the symbol rate it measured.
fn first_look(out_rate: f64) -> usize {
    ((out_rate * 0.6) as usize).clamp((out_rate * 2.0).min(300_000.0) as usize, 2_000_000)
}

/// Samples holding three of the longest PLFRAMEs at `rs` — what it takes to
/// see a header confirmed on its frame grid, with margin.
fn three_frames(out_rate: f64, rs: f64) -> usize {
    let symbols = 3.0 * decdvb_frame::MAX_PLFRAME_LEN as f64 * 1.2;
    (symbols * out_rate / rs) as usize
}

/// Pause between Identify checks once it has a confident answer.
const IDENTIFY_REST: std::time::Duration = std::time::Duration::from_secs(3);

enum Decoder {
    Identify {
        buf: Vec<Iq>,
        rest_until: Option<Instant>,
        /// Samples to gather for the current check.
        target: usize,
        /// The last result came from too short a listen to rule DVB-S2 out.
        provisional: bool,
    },
    Spectrum,
    Record {
        writer: Option<IqFileWriter>,
        path: PathBuf,
        bytes: u64,
    },
    Dvbs2 {
        buf: Vec<Iq>,
        demod: Option<Box<Demod>>,
    },
}

struct Worker {
    in_rate: f64,
    settings: VfoSettings,
    ddc: Ddc,
    decoder: Decoder,
    spec: Spectrum,
    status: Arc<Mutex<VfoStatus>>,
    dropped: Arc<AtomicU64>,
    load: f32,
    bb: Vec<Iq>,
    frames: Vec<crate::demod::PlFrame>,
    modcods: BTreeMap<u8, u64>,
    last_modcod: Option<u8>,
    identification: Option<Identification>,
}

impl Worker {
    fn new(
        in_rate: f64,
        settings: VfoSettings,
        status: Arc<Mutex<VfoStatus>>,
        dropped: Arc<AtomicU64>,
    ) -> Self {
        let ddc = Ddc::new(
            in_rate,
            settings.offset_hz,
            settings.bandwidth_hz.min(in_rate),
        );
        let decoder = Self::make_decoder(&settings, &ddc);
        Worker {
            in_rate,
            ddc,
            decoder,
            spec: Spectrum::new(1024),
            settings,
            status,
            dropped,
            load: 0.0,
            bb: Vec::new(),
            frames: Vec::new(),
            modcods: BTreeMap::new(),
            last_modcod: None,
            identification: None,
        }
    }

    fn make_decoder(s: &VfoSettings, ddc: &Ddc) -> Decoder {
        match s.decoder {
            DecoderKind::Identify => Decoder::Identify {
                buf: Vec::new(),
                rest_until: None,
                target: first_look(ddc.out_rate()),
                provisional: false,
            },
            DecoderKind::Spectrum => Decoder::Spectrum,
            DecoderKind::IqRecord => {
                let stamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let path = s.record_dir.join(format!(
                    "decdvb-{}-{:+.0}Hz-{:.0}Sps-{stamp}.cf32",
                    s.name.replace(' ', "_"),
                    s.offset_hz,
                    ddc.out_rate()
                ));
                let _ = std::fs::create_dir_all(&s.record_dir);
                let writer = IqFileWriter::create(&path, SampleFormat::Cf32).ok();
                Decoder::Record {
                    writer,
                    path,
                    bytes: 0,
                }
            }
            DecoderKind::Dvbs2Ip | DecoderKind::Dvbs2Ts => Decoder::Dvbs2 {
                buf: Vec::new(),
                demod: None,
            },
        }
    }

    fn apply(&mut self, new: VfoSettings) {
        let rebuild_ddc = new.bandwidth_hz != self.settings.bandwidth_hz;
        let retuned = new.offset_hz != self.settings.offset_hz;
        let new_decoder = new.decoder != self.settings.decoder
            || new.symbol_rate != self.settings.symbol_rate
            || new.gold_code != self.settings.gold_code;
        if rebuild_ddc {
            self.ddc = Ddc::new(
                self.in_rate,
                new.offset_hz,
                new.bandwidth_hz.min(self.in_rate),
            );
        } else if retuned {
            // Retune in place: the filter keeps its history, so dragging is smooth.
            self.ddc.set_offset(new.offset_hz);
        }
        // An enable/disable or a rename changes nothing about the signal; a
        // retune does. Without the reset an Identify VFO dragged onto another
        // carrier kept its gathered signal and analysed a mixture of the two.
        let signal_changed = rebuild_ddc || retuned || new_decoder;
        self.settings = new;
        // A recorder keeps its file across a retune (a new file per drag frame
        // would be worse); everything that analyses the signal starts afresh.
        let analyses = !matches!(self.decoder, Decoder::Record { .. } | Decoder::Spectrum);
        if (signal_changed && analyses) || new_decoder {
            self.decoder = Self::make_decoder(&self.settings, &self.ddc);
            self.modcods.clear();
            self.last_modcod = None;
            self.identification = None;
        }
    }

    fn run(
        mut self,
        rx: Receiver<VfoMsg>,
        mailbox: Arc<Mutex<Option<VfoSettings>>>,
        stop: Arc<AtomicBool>,
    ) {
        while let Ok(msg) = rx.recv() {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let pending = mailbox.lock().unwrap().take();
            if let Some(s) = pending {
                self.apply(s);
                self.publish(0);
            }
            match msg {
                VfoMsg::Wake => {}
                VfoMsg::Block(block) => {
                    let t0 = Instant::now();
                    self.bb.clear();
                    self.ddc.process(&block, &mut self.bb);
                    let n = self.bb.len();
                    self.decode();
                    // Load: processing time over the block's real duration.
                    let real = block.len() as f64 / self.in_rate;
                    let used = t0.elapsed().as_secs_f64() / real.max(1e-9);
                    self.load = 0.9 * self.load + 0.1 * used as f32;
                    self.publish(n);
                }
            }
        }
        if let Decoder::Record {
            writer: Some(w), ..
        } = std::mem::replace(&mut self.decoder, Decoder::Spectrum)
        {
            let _ = w.finish();
        }
    }

    fn decode(&mut self) {
        let out_rate = self.ddc.out_rate();
        match &mut self.decoder {
            Decoder::Spectrum => {}
            Decoder::Record { writer, bytes, .. } => {
                if let Some(w) = writer {
                    if w.write(&self.bb).is_ok() {
                        *bytes += self.bb.len() as u64 * 8;
                    } else {
                        *writer = None;
                    }
                }
            }
            Decoder::Identify {
                buf,
                rest_until,
                target,
                provisional,
            } => {
                // After a confident result, rest before checking again: a
                // carrier rarely changes, and re-analysing millions of samples
                // back to back was most of a wide Identify VFO's CPU. A retune
                // resets this (see `apply`), so moving the VFO answers at once.
                if rest_until.is_some_and(|t| Instant::now() < t) {
                    return;
                }
                buf.extend_from_slice(&self.bb);
                if buf.len() >= *target {
                    let id = identify_in(buf, out_rate, Some(self.settings.bandwidth_hz));
                    let is_s2 = matches!(id.verdict, Verdict::DvbS2(_));
                    let needed = id.symbol_rate.map(|rs| three_frames(out_rate, rs));
                    match needed {
                        // Not DVB-S2, but the listen was too short to see three
                        // frames at this symbol rate: say so, and listen longer
                        // straight away.
                        Some(n) if !is_s2 && buf.len() < n => {
                            *provisional = true;
                            *target = n.min(8_000_000);
                            *rest_until = None;
                        }
                        _ => {
                            *provisional = false;
                            *target = needed
                                .unwrap_or(*target)
                                .clamp(first_look(out_rate), 8_000_000);
                            *rest_until = Some(Instant::now() + IDENTIFY_REST);
                        }
                    }
                    self.identification = Some(id);
                    buf.clear();
                }
            }
            Decoder::Dvbs2 { buf, demod } => match demod {
                Some(d) => {
                    self.frames.clear();
                    d.process(&self.bb, &mut self.frames);
                    for f in &self.frames {
                        *self.modcods.entry(f.pls.modcod).or_default() += 1;
                        self.last_modcod = Some(f.pls.modcod);
                    }
                }
                None => {
                    buf.extend_from_slice(&self.bb);
                    if buf.len() >= gather_target(out_rate) {
                        let id = identify_in(buf, out_rate, Some(self.settings.bandwidth_hz));
                        let rs = self.settings.symbol_rate.or(id.symbol_rate);
                        let alpha = id.roll_off.map_or(0.35, |r| r.as_f64());
                        if let Some(rs) = rs.filter(|&r| out_rate / r >= 2.0) {
                            // Start demodulating, from the gathered signal on.
                            let mut d =
                                Box::new(Demod::new(out_rate, rs, alpha, self.settings.gold_code));
                            // The carrier may sit off the VFO centre; the
                            // demodulator expects it at DC. Shift what was
                            // gathered, and retune the DDC onto the carrier so
                            // every later block arrives centred too.
                            let mut shifted = std::mem::take(buf);
                            shift(&mut shifted, out_rate, id.center_offset_hz);
                            self.ddc
                                .set_offset(self.settings.offset_hz + id.center_offset_hz);
                            self.frames.clear();
                            d.process(&shifted, &mut self.frames);
                            *demod = Some(d);
                        } else {
                            buf.clear();
                        }
                        self.identification = Some(id);
                    }
                }
            },
        }
    }

    fn publish(&mut self, new_samples: usize) {
        let out_rate = self.ddc.out_rate();
        let spectrum_db = if self.bb.len() >= 1024 {
            Some(self.spec.compute_max(&self.bb, 16))
        } else {
            None
        };
        let level_db = if self.bb.is_empty() {
            None
        } else {
            let p = self.bb.iter().map(|s| s.norm_sqr()).sum::<f32>() / self.bb.len() as f32;
            Some(10.0 * p.max(1e-20).log10())
        };

        let mut st = self.status.lock().unwrap();
        st.out_rate = out_rate;
        st.decimation = self.ddc.decimation();
        st.load = self.load;
        st.dropped = self.dropped.load(Ordering::Relaxed);
        if let Some(s) = spectrum_db {
            st.spectrum_db = s;
        }
        if let Some(l) = level_db {
            st.level_db = l;
        }
        st.identification = self.identification.clone();
        st.modcods = self.modcods.clone();
        st.last_modcod = self.last_modcod;
        st.progress = 0.0;
        st.lock = None;
        st.recording = None;

        match &self.decoder {
            Decoder::Identify {
                buf,
                target,
                provisional,
                ..
            } => {
                st.progress = buf.len() as f32 / (*target).max(1) as f32;
                st.provisional = *provisional;
                st.message = match &self.identification {
                    Some(id) if *provisional => format!(
                        "{} — provisional: listening longer for frame headers",
                        id.summary()
                    ),
                    Some(id) => id.summary(),
                    None => "listening…".into(),
                };
                st.symbol_rate = self.identification.as_ref().and_then(|i| i.symbol_rate);
                if new_samples > 0 {
                    let stride = (self.bb.len() / 2000).max(1);
                    st.scatter = self.bb.iter().step_by(stride).copied().collect();
                }
            }
            Decoder::Spectrum => {
                st.message = format!("{:.1} dB", st.level_db);
                if new_samples > 0 {
                    let stride = (self.bb.len() / 2000).max(1);
                    st.scatter = self.bb.iter().step_by(stride).copied().collect();
                }
            }
            Decoder::Record {
                writer,
                path,
                bytes,
            } => {
                st.recording = Some((path.clone(), *bytes));
                st.message = if writer.is_some() {
                    format!("recording {:.1} MB", *bytes as f64 / 1e6)
                } else {
                    format!("cannot write {}", path.display())
                };
            }
            Decoder::Dvbs2 { buf, demod } => match demod {
                None => {
                    st.progress = buf.len() as f32 / gather_target(out_rate) as f32;
                    st.message = match &self.identification {
                        Some(id) if matches!(id.verdict, Verdict::NoSignal) => {
                            "no signal in this VFO".into()
                        }
                        Some(id) => format!("no symbol rate yet — {}", id.summary()),
                        None => "acquiring: finding the symbol rate…".into(),
                    };
                }
                Some(d) => {
                    st.lock = Some(d.lock_state());
                    st.frames = d.frames();
                    st.lock_losses = d.losses();
                    st.symbol_rate = Some(d.symbol_rate());
                    st.scatter = d.recent_symbols(2000).to_vec();
                    st.message = match d.lock_state() {
                        LockState::Searching => "searching for PLHEADERs".into(),
                        LockState::Found => "found a PLHEADER, confirming".into(),
                        LockState::Locked => format!(
                            "locked, {} frames — FEC and {} output arrive in M2–M4",
                            d.frames(),
                            if self.settings.decoder == DecoderKind::Dvbs2Ts {
                                "TS"
                            } else {
                                "GSE/IP"
                            }
                        ),
                    };
                }
            },
        }
    }
}

/// Rotate baseband by `-freq_hz` in place.
fn shift(x: &mut [Iq], rate: f64, freq_hz: f64) {
    if freq_hz == 0.0 {
        return;
    }
    let w = -std::f64::consts::TAU * freq_hz / rate;
    for (n, s) in x.iter_mut().enumerate() {
        let ph = w * n as f64;
        *s *= Iq::new(ph.cos() as f32, ph.sin() as f32);
    }
}
