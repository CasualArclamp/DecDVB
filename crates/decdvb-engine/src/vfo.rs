//! VFOs: a centre, a bandwidth and a decoder, each on its own worker thread.
//!
//! The wideband front end hands every VFO the same blocks (shared through an
//! `Arc`, so fanning out costs one pointer per VFO, not a copy). A worker that
//! falls behind has blocks dropped and counted rather than stalling the front
//! end — the waterfall and the other VFOs keep running in real time.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufWriter, Write};
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
use crate::psk::PskDemod;
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
    /// Generic PSK/APSK: lock any linearly modulated carrier and write its
    /// hard-decided symbols to a `.bin` file, one byte per symbol.
    PskSymbols,
    /// Record the VFO's narrowband IQ.
    IqRecord,
    /// Zoomed spectrum and level only.
    Spectrum,
}

impl DecoderKind {
    pub const ALL: [DecoderKind; 6] = [
        DecoderKind::Identify,
        DecoderKind::Dvbs2Ip,
        DecoderKind::Dvbs2Ts,
        DecoderKind::PskSymbols,
        DecoderKind::IqRecord,
        DecoderKind::Spectrum,
    ];

    pub fn label(self) -> &'static str {
        match self {
            DecoderKind::Identify => "Identify (what is this?)",
            DecoderKind::Dvbs2Ip => "DVB-S2/S2X → GSE/IP (PCAP)",
            DecoderKind::Dvbs2Ts => "DVB-S2/S2X → MPEG-TS",
            DecoderKind::PskSymbols => "Generic PSK → symbols (.bin)",
            DecoderKind::IqRecord => "IQ recorder",
            DecoderKind::Spectrum => "Spectrum only",
        }
    }

    pub fn short(self) -> &'static str {
        match self {
            DecoderKind::Identify => "ID",
            DecoderKind::Dvbs2Ip => "S2→IP",
            DecoderKind::Dvbs2Ts => "S2→TS",
            DecoderKind::PskSymbols => "PSK",
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
    /// Generic PSK: the constellation to decide on; `None` takes Identify's.
    pub psk_modulation: Option<decdvb_core::Modulation>,
    /// Generic PSK: write symbols to a file. Off by default — the decoder
    /// shows the locked constellation until recording is asked for.
    pub record: bool,
    /// Where the IQ recorder and the symbol writer write.
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
            psk_modulation: None,
            record: false,
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
    /// File being written and its size so far: bytes of IQ for the recorder,
    /// symbols (one byte each) for the generic PSK decoder.
    pub recording: Option<(PathBuf, u64)>,
    /// `recording` is being written now (not just the last file).
    pub recording_active: bool,
    /// Carrier loop of a running demodulator.
    pub carrier: Option<CarrierState>,
}

/// A running demodulator's carrier loop, for display.
#[derive(Debug, Clone, Copy)]
pub struct CarrierState {
    pub locked: bool,
    pub mer_db: f32,
    /// Residual offset the loop is tracking, Hz.
    pub offset_hz: f64,
    pub modulation: decdvb_core::Modulation,
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

/// A first look: ~0.6 s, but never more than 2 s of signal, so a narrow VFO
/// answers quickly (a 10 kBd carrier gives 20 000 symbols, plenty for the
/// symbol rate). For Identify, whether that was long enough to judge DVB-S2 is
/// decided afterwards, from the symbol rate it measured; the demodulators
/// only need the symbol rate from it, and find frames themselves.
fn first_look(out_rate: f64) -> usize {
    ((out_rate * 0.6) as usize).clamp((out_rate * 2.0).min(300_000.0) as usize, 2_000_000)
}

/// Samples holding two of the longest PLFRAMEs at `rs`, with margin — what it
/// takes to see one header confirmed on its frame grid wherever in the frame
/// the listen happens to start.
fn enough_frames(out_rate: f64, rs: f64) -> usize {
    let symbols = 2.2 * decdvb_frame::MAX_PLFRAME_LEN as f64;
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
    Psk {
        buf: Vec<Iq>,
        demod: Option<Box<PskDemod>>,
        /// Open while recording.
        writer: Option<BufWriter<File>>,
        /// The current or last file, and the symbols in it.
        path: Option<PathBuf>,
        written: u64,
        /// Opening the file failed; shown until recording is turned off.
        open_failed: bool,
        /// The carrier's offset in the band, for file names.
        carrier_hz: f64,
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
    syms: Vec<(u8, Iq)>,
    sym_bytes: Vec<u8>,
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
            syms: Vec::new(),
            sym_bytes: Vec::new(),
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
                let path = s.record_dir.join(format!(
                    "decdvb-{}-{:+.0}Hz-{:.0}Sps-{}.cf32",
                    s.name.replace(' ', "_"),
                    s.offset_hz,
                    ddc.out_rate(),
                    unix_stamp()
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
            DecoderKind::PskSymbols => Decoder::Psk {
                buf: Vec::new(),
                demod: None,
                writer: None,
                path: None,
                written: 0,
                open_failed: false,
                carrier_hz: s.offset_hz,
            },
        }
    }

    fn apply(&mut self, new: VfoSettings) {
        let rebuild_ddc = new.bandwidth_hz != self.settings.bandwidth_hz;
        let retuned = new.offset_hz != self.settings.offset_hz;
        let new_decoder = new.decoder != self.settings.decoder
            || new.symbol_rate != self.settings.symbol_rate
            || new.gold_code != self.settings.gold_code
            || new.psk_modulation != self.settings.psk_modulation;
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
        match std::mem::replace(&mut self.decoder, Decoder::Spectrum) {
            Decoder::Record {
                writer: Some(w), ..
            } => {
                let _ = w.finish();
            }
            Decoder::Psk {
                writer: Some(mut w),
                ..
            } => {
                let _ = w.flush();
            }
            _ => {}
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
                    let needed = id.symbol_rate.map(|rs| enough_frames(out_rate, rs));
                    match needed {
                        // Not DVB-S2, but the listen was too short to see two
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
                    if buf.len() >= first_look(out_rate) {
                        let id = identify_in(buf, out_rate, Some(self.settings.bandwidth_hz));
                        let rs = self.settings.symbol_rate.or(id.symbol_rate);
                        let alpha = id.roll_off.map_or(0.35, |r| r.as_f64());
                        if let Some(rs) = rs.filter(|&r| out_rate / r >= 2.0) {
                            // Start demodulating, from the gathered signal on.
                            let mut d = Demod::new(out_rate, rs, alpha, self.settings.gold_code);
                            // Identify's offset is averaged over every header
                            // it saw: the best seed for the carrier loop.
                            if let Some(f) = id.carrier_offset_hz {
                                d = d.with_carrier_offset(f / rs);
                            }
                            let mut d = Box::new(d);
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
            Decoder::Psk {
                buf,
                demod,
                writer,
                path,
                written,
                open_failed,
                carrier_hz,
            } => match demod {
                Some(d) => {
                    self.syms.clear();
                    d.process(&self.bb, &mut self.syms);
                    let file = PskFile {
                        writer,
                        path,
                        written,
                        open_failed,
                    };
                    file.follow(&self.settings, d, *carrier_hz);
                    write_symbols(writer, written, &self.syms, &mut self.sym_bytes);
                }
                None => {
                    buf.extend_from_slice(&self.bb);
                    if buf.len() >= first_look(out_rate) {
                        let id = identify_in(buf, out_rate, Some(self.settings.bandwidth_hz));
                        let rs = self.settings.symbol_rate.or(id.symbol_rate);
                        let alpha = id.roll_off.map_or(0.35, |r| r.as_f64());
                        let usable = !matches!(id.verdict, Verdict::NoSignal);
                        if let Some(rs) = rs.filter(|&r| usable && out_rate / r >= 2.0) {
                            let modulation = self
                                .settings
                                .psk_modulation
                                .or(id.constellation.map(|c| c.modulation()))
                                .unwrap_or(decdvb_core::Modulation::Qpsk);
                            // Identify's residual offset seeds the carrier loop.
                            let seed = id.carrier_offset_hz.unwrap_or(0.0) / rs;
                            let mut d =
                                Box::new(PskDemod::new(out_rate, rs, alpha, modulation, seed));
                            // Centre the carrier, as for DVB-S2 above.
                            let mut shifted = std::mem::take(buf);
                            shift(&mut shifted, out_rate, id.center_offset_hz);
                            self.ddc
                                .set_offset(self.settings.offset_hz + id.center_offset_hz);

                            *carrier_hz = self.settings.offset_hz + id.center_offset_hz;

                            self.syms.clear();
                            d.process(&shifted, &mut self.syms);
                            // Recording armed before lock starts at once.
                            let file = PskFile {
                                writer,
                                path,
                                written,
                                open_failed,
                            };
                            file.follow(&self.settings, &d, *carrier_hz);
                            write_symbols(writer, written, &self.syms, &mut self.sym_bytes);
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
        st.recording_active = false;
        st.carrier = None;

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
                // The carrier-locked symbols of the last identification; raw
                // baseband only until there is one.
                match self
                    .identification
                    .as_ref()
                    .filter(|i| !i.symbols.is_empty())
                {
                    Some(id) => st.scatter = id.symbols.clone(),
                    None if new_samples > 0 => {
                        let stride = (self.bb.len() / 2000).max(1);
                        st.scatter = self.bb.iter().step_by(stride).copied().collect();
                    }
                    None => {}
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
                st.recording_active = writer.is_some();
                st.message = if writer.is_some() {
                    format!("recording {:.1} MB", *bytes as f64 / 1e6)
                } else {
                    format!("cannot write {}", path.display())
                };
            }
            Decoder::Dvbs2 { buf, demod } => match demod {
                None => {
                    st.progress = buf.len() as f32 / first_look(out_rate) as f32;
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
                    st.scatter = d.recent_symbols(2000);
                    if let (true, Some(m)) = (d.carrier_running(), d.modulation()) {
                        st.carrier = Some(CarrierState {
                            locked: d.carrier_locked(),
                            mer_db: d.mer_db(),
                            offset_hz: d.carrier_offset_hz(),
                            modulation: m,
                        });
                    }
                    st.message = match d.lock_state() {
                        LockState::Searching => "searching for PLHEADERs".into(),
                        LockState::Found => "found a PLHEADER, confirming".into(),
                        LockState::Locked => format!(
                            "locked, {} frames, MER {:.1} dB — FEC and {} output arrive in M2–M4",
                            d.frames(),
                            d.mer_db(),
                            if self.settings.decoder == DecoderKind::Dvbs2Ts {
                                "TS"
                            } else {
                                "GSE/IP"
                            }
                        ),
                    };
                }
            },
            Decoder::Psk {
                buf,
                demod,
                writer,
                path,
                written,
                open_failed,
                ..
            } => match demod {
                None => {
                    st.progress = buf.len() as f32 / first_look(out_rate) as f32;
                    st.message = match &self.identification {
                        Some(id) if matches!(id.verdict, Verdict::NoSignal) => {
                            "no signal in this VFO".into()
                        }
                        Some(id) => format!("no symbol rate yet — {}", id.summary()),
                        None => "acquiring: finding the symbol rate…".into(),
                    };
                }
                Some(d) => {
                    st.symbol_rate = Some(d.symbol_rate());
                    st.scatter = d.recent();
                    st.carrier = Some(CarrierState {
                        locked: d.locked(),
                        mer_db: d.mer_db(),
                        offset_hz: d.carrier_offset_hz(),
                        modulation: d.modulation(),
                    });
                    if let Some(p) = path {
                        st.recording = Some((p.clone(), *written));
                    }
                    st.recording_active = writer.is_some();
                    let lock = format!(
                        "{} {}, MER {:.1} dB",
                        d.modulation().name(),
                        if d.locked() { "locked" } else { "not locked" },
                        d.mer_db()
                    );
                    st.message = match (writer.is_some(), *open_failed, path) {
                        (_, true, Some(p)) => format!("{lock} — cannot write {}", p.display()),
                        (true, _, _) => format!("{lock} — recording, {written} symbols"),
                        _ => format!("{lock} — press Record to write symbols"),
                    };
                }
            },
        }
    }
}

/// The generic PSK decoder's output file, borrowed from its decoder state.
struct PskFile<'a> {
    writer: &'a mut Option<BufWriter<File>>,
    path: &'a mut Option<PathBuf>,
    written: &'a mut u64,
    open_failed: &'a mut bool,
}

impl PskFile<'_> {
    /// Open or close the file to match `settings.record`. A new recording is
    /// a new file; stopping keeps the last path and count for display.
    fn follow(self, settings: &VfoSettings, d: &PskDemod, carrier_hz: f64) {
        match (settings.record, self.writer.is_some()) {
            (true, false) if !*self.open_failed => {
                let p = settings.record_dir.join(format!(
                    "decdvb-{}-{:+.0}Hz-{:.0}Bd-{}-{}.bin",
                    settings.name.replace(' ', "_"),
                    carrier_hz,
                    d.symbol_rate(),
                    d.modulation().name().replace('/', ""),
                    unix_stamp()
                ));
                let _ = std::fs::create_dir_all(&settings.record_dir);
                *self.writer = File::create(&p).ok().map(BufWriter::new);
                *self.open_failed = self.writer.is_none();
                *self.written = 0;
                *self.path = Some(p);
            }
            (false, true) => {
                if let Some(mut w) = self.writer.take() {
                    let _ = w.flush();
                }
            }
            (false, false) => *self.open_failed = false,
            _ => {}
        }
    }
}

/// Seconds since the Unix epoch, for file names.
fn unix_stamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Append hard decisions to the symbol file, one byte per symbol: the
/// symbol's bit label under the DVB-S2 mapping (EN 302 307-1 §5.4; BPSK 0 ->
/// +1). A write error closes the file rather than retrying every block.
fn write_symbols(
    writer: &mut Option<BufWriter<File>>,
    written: &mut u64,
    syms: &[(u8, Iq)],
    scratch: &mut Vec<u8>,
) {
    let Some(w) = writer else { return };
    scratch.clear();
    scratch.extend(syms.iter().map(|&(i, _)| i));
    if w.write_all(scratch).is_ok() {
        *written += syms.len() as u64;
    } else {
        *writer = None;
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

#[cfg(test)]
mod tests {
    use super::*;
    use decdvb_fec::Constellation;
    use decdvb_mod::Shaper;

    /// Drive a worker the way `run` does, without the thread.
    fn feed(w: &mut Worker, x: &[Iq], block: usize) {
        for c in x.chunks(block) {
            w.bb.clear();
            w.ddc.process(c, &mut w.bb);
            let n = w.bb.len();
            w.decode();
            w.publish(n);
        }
    }

    /// A carrier of `cst` symbols at `rs`, `sps` samples per symbol (the band
    /// is `rs * sps` wide), `f_hz` off the band centre, with a random-walk
    /// phase noise of `walk` rad per symbol (rms) and a little white noise.
    fn carrier(
        cst: &Constellation,
        n_sym: usize,
        sps: usize,
        rs: f64,
        f_hz: f64,
        walk: f64,
        seed: u64,
    ) -> Vec<Iq> {
        let mut s = seed | 1;
        let mut uniform = move || {
            s ^= s >> 12;
            s ^= s << 25;
            s ^= s >> 27;
            (s.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
        };
        let m = cst.points.len();
        let syms: Vec<Iq> = (0..n_sym)
            .map(|_| cst.map((uniform() * m as f64) as usize % m))
            .collect();
        let mut sh = Shaper::new(sps, 0.25, 16);
        let mut x = Vec::new();
        sh.process(&syms, &mut x);
        let w = std::f64::consts::TAU * f_hz / (rs * sps as f64);
        let mut drift = 0.0;
        for (n, v) in x.iter_mut().enumerate() {
            if n % sps == 0 {
                drift += walk * 12f64.sqrt() * (uniform() - 0.5);
            }
            let ph = w * n as f64 + drift;
            let noise = Iq::new(uniform() as f32 - 0.5, uniform() as f32 - 0.5) * 0.02;
            *v = *v * Iq::new(ph.cos() as f32, ph.sin() as f32) + noise;
        }
        x
    }

    /// Run a generic PSK VFO over `x`; return its status and output folder.
    fn psk_vfo(
        rate: f64,
        x: &[Iq],
        offset_hz: f64,
        bandwidth_hz: f64,
        tag: &str,
    ) -> (VfoStatus, PathBuf) {
        let dir = std::env::temp_dir().join(format!("decdvb-{tag}-{}", std::process::id()));
        let mut settings =
            VfoSettings::new("PSK test", offset_hz, bandwidth_hz, DecoderKind::PskSymbols);
        settings.record_dir = dir.clone();
        settings.record = true;
        let status = Arc::new(Mutex::new(VfoStatus::default()));
        let mut wk = Worker::new(rate, settings, status.clone(), Arc::new(AtomicU64::new(0)));
        feed(&mut wk, x, 65_536);
        drop(wk); // flushes the file
        let st = status.lock().unwrap().clone();
        (st, dir)
    }

    #[test]
    fn psk_vfo_locks_and_writes_one_byte_per_symbol() {
        // 8PSK at 62.5 kBd, 40 kHz off the band centre plus 310 Hz the VFO is
        // not told about, in a 500 kS/s band.
        let rs = 62_500.0;
        let x = carrier(&Constellation::psk8(), 200_000, 8, rs, 40_310.0, 0.0, 7);
        let (st, dir) = psk_vfo(500_000.0, &x, 38_000.0, 110_000.0, "psk-vfo");

        let c = st.carrier.expect("demodulator running");
        assert_eq!(
            c.modulation,
            decdvb_core::Modulation::Psk8,
            "{}",
            st.message
        );
        assert!(c.locked, "{}", st.message);
        assert!(c.mer_db > 20.0, "{}", st.message);
        let tracked = st.symbol_rate.unwrap();
        assert!((tracked - rs).abs() < rs * 1e-3, "symbol rate {tracked}");
        let (path, written) = st.recording.expect("symbol file");
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(written > 100_000, "only {written} symbols");
        assert_eq!(bytes.len() as u64, written);
        assert!(bytes.iter().all(|&b| b < 8));
        assert!(path.to_string_lossy().ends_with(".bin"));
    }

    #[test]
    fn psk_vfo_writes_nothing_until_record_is_pressed() {
        let rs = 62_500.0;
        let x = carrier(&Constellation::qpsk(), 150_000, 8, rs, 40_000.0, 0.0, 11);
        let dir = std::env::temp_dir().join(format!("decdvb-psk-arm-{}", std::process::id()));
        let mut settings =
            VfoSettings::new("PSK arm", 40_000.0, 110_000.0, DecoderKind::PskSymbols);
        settings.record_dir = dir.clone();
        let status = Arc::new(Mutex::new(VfoStatus::default()));
        let mut wk = Worker::new(500_000.0, settings.clone(), status.clone(), Arc::new(AtomicU64::new(0)));
        let third = x.len() / 3;

        // Locked and showing symbols, but no file.
        feed(&mut wk, &x[..third], 65_536);
        {
            let st = status.lock().unwrap();
            assert!(st.carrier.is_some_and(|c| c.locked), "{}", st.message);
            assert!(!st.scatter.is_empty());
            assert!(st.recording.is_none() && !st.recording_active);
        }
        assert!(!dir.exists(), "a file was written before Record");

        // Record: a file grows, without restarting the demodulator.
        settings.record = true;
        wk.apply(settings.clone());
        feed(&mut wk, &x[third..2 * third], 65_536);
        let (path, n) = {
            let st = status.lock().unwrap();
            assert!(st.recording_active, "{}", st.message);
            st.recording.clone().unwrap()
        };
        assert!(n > 10_000, "only {n} symbols");

        // Stop: the file is closed; the last one stays on show.
        settings.record = false;
        wk.apply(settings);
        feed(&mut wk, &x[2 * third..], 65_536);
        let st = status.lock().unwrap().clone();
        assert!(!st.recording_active);
        assert_eq!(st.recording, Some((path.clone(), n)));
        let len = std::fs::metadata(&path).unwrap().len();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(len, n);
    }

    #[test]
    fn psk_vfo_locks_a_10_kbaud_carrier_with_lnb_drift() {
        // A narrow Ku-band SCPC carrier: QPSK at 10 kBd, 150 Hz from where the
        // VFO was dropped (LNB error), with phase noise, in a 250 kS/s band.
        let rs = 10_000.0;
        let x = carrier(&Constellation::qpsk(), 50_000, 25, rs, 31_150.0, 0.01, 9);
        let (st, dir) = psk_vfo(250_000.0, &x, 31_000.0, 20_000.0, "psk-narrow");
        let _ = std::fs::remove_dir_all(&dir);

        let Some(c) = st.carrier else {
            panic!(
                "not demodulating: {} (rate {:.0}, progress {}, {} samples in)",
                st.message,
                st.out_rate,
                st.progress,
                x.len()
            )
        };
        assert_eq!(
            c.modulation,
            decdvb_core::Modulation::Qpsk,
            "{}",
            st.message
        );
        assert!(c.locked, "{}", st.message);
        assert!(c.mer_db > 15.0, "{}", st.message);
        let tracked = st.symbol_rate.unwrap();
        assert!((tracked - rs).abs() < rs * 2e-3, "symbol rate {tracked}");
        assert!(
            st.recording.is_some_and(|(_, n)| n > 10_000),
            "{}",
            st.message
        );
    }
}
