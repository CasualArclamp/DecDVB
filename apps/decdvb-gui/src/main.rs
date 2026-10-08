// Hide the console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! DecDVB desktop GUI: an SDR++-style wideband waterfall with VFOs, each
//! running a decoder of your choice.

mod automation;
mod band_view;
mod filename;
mod format;
mod freq_display;
mod player;
mod prefs;
mod radio;
mod side_panel;
mod ts_viewer;
mod waterfall;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use decdvb_core::SampleFormat;
use decdvb_engine::{
    DecoderKind, Engine, EngineOptions, FrontStatus, SourceState, VfoId, VfoStatus,
};
use decdvb_io::IqFileReader;
use eframe::egui::{self, RichText};

use band_view::{Action, BandInput, BandView, UiVfo};
use waterfall::{History, RingImage};

fn main() -> eframe::Result {
    // `--version` alone: print it and stop (the release workflow checks it).
    // `env!` reads a variable at compile time; Cargo sets this one from
    // `[workspace.package].version`.
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("decdvb-gui {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    prefs::load();
    let opts = match automation::Options::from_args() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("decdvb-gui: {e}");
            eprintln!(
                "usage: decdvb-gui [capture] [--claim-carriers] [--decoder id|ip|ts|psk|rec|spec] \
                 [--select N] [--after SECS] [--screenshot OUT.png]"
            );
            std::process::exit(2);
        }
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1400.0, 860.0])
            .with_min_inner_size([900.0, 560.0])
            .with_title("DecDVB")
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(
        "DecDVB",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_theme(egui::Theme::Dark);
            // egui's proportional font has no arrows or ●; its bundled
            // monospace font does, so let text fall back to it.
            let mut fonts = egui::FontDefinitions::default();
            if let Some(f) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
                f.push("Hack".into());
            }
            cc.egui_ctx.set_fonts(fonts);
            let mut app = App {
                automation: automation::Automation::new(&opts),
                ..App::default()
            };
            // Claimed VFOs keep `VfoSettings::new`'s temp-dir output folder.
            if let Some(k) = opts.decoder {
                app.new_decoder = k;
            }
            if let Some(p) = opts.file.clone() {
                app.open_file(p);
            }
            #[cfg(feature = "hackrf")]
            if let Some(mhz) = opts.hackrf_mhz {
                app.radio.settings.center_hz = (mhz * 1e6).round() as u64;
                if let Some(r) = opts.rate_msps {
                    app.radio.settings.sample_rate = (r * 1e6).round() as u32;
                }
                app.radio.lnb_lo_mhz = opts.lo_mhz.unwrap_or(0.0);
                app.start_hackrf();
            }
            Ok(Box::new(app))
        }),
    )
}

struct App {
    engine: Option<Engine>,
    path: Option<PathBuf>,
    /// Source settings as the user has them, and as the running engine has them.
    sample_rate: f64,
    format: SampleFormat,
    rf_center_mhz: f64,
    opts: EngineOptions,
    applied: Option<(f64, SampleFormat, usize, bool, bool)>,
    history: History,
    ring: RingImage,
    last_seq: u64,
    front: FrontStatus,
    band: BandView,
    vfos: Vec<UiVfo>,
    statuses: BTreeMap<VfoId, VfoStatus>,
    selected: Option<VfoId>,
    new_decoder: DecoderKind,
    names: u32,
    paused: bool,
    ts_viewer: ts_viewer::TsViewer,
    /// A player to open on a multicast audio stream once it is relayed.
    pending_audio: Option<(
        VfoId,
        std::net::SocketAddr,
        player::Player,
        std::time::Instant,
    )>,
    /// A player to open once its VFO's TCP server is up.
    pending_player: Option<(VfoId, player::Player, std::time::Instant)>,
    note: String,
    automation: automation::Automation,
    #[cfg(feature = "hackrf")]
    radio: radio::RadioPanel,
    /// Live controls while the HackRF is the source.
    #[cfg(feature = "hackrf")]
    hackrf: Option<decdvb_io::HackRfControl>,
}

impl Default for App {
    fn default() -> Self {
        App {
            engine: None,
            path: None,
            sample_rate: 2e6,
            format: SampleFormat::Cs8,
            rf_center_mhz: 0.0,
            opts: EngineOptions::default(),
            applied: None,
            history: History::default(),
            ring: RingImage::default(),
            last_seq: 0,
            front: FrontStatus::default(),
            band: BandView::default(),
            vfos: Vec::new(),
            statuses: BTreeMap::new(),
            selected: None,
            new_decoder: DecoderKind::Identify,
            names: 1,
            paused: false,
            pending_player: None,
            pending_audio: None,
            ts_viewer: Default::default(),
            note: String::new(),
            automation: automation::Automation::new(&automation::Options::default()),
            #[cfg(feature = "hackrf")]
            radio: radio::RadioPanel::default(),
            #[cfg(feature = "hackrf")]
            hackrf: None,
        }
    }
}

impl App {
    fn next_name(&self) -> String {
        format!("VFO {}", self.names)
    }

    /// RF frequency at the span's centre. With no file open this is the
    /// radio's tuning (plus LO), whether or not it is running yet.
    fn rf_center(&self) -> f64 {
        #[cfg(feature = "hackrf")]
        if self.path.is_none() {
            return self.radio.rf_center();
        }
        self.rf_center_mhz * 1e6
    }

    /// Run `source` through a fresh engine, keeping the VFOs.
    fn start_engine(&mut self, source: Box<dyn decdvb_io::IqSource>) {
        let rate = source.sample_rate();
        let mut eng = Engine::start(source, self.opts.clone());
        if self.paused {
            eng.set_paused(true);
        }
        // Re-create the VFOs; the new engine numbers them afresh.
        let old = std::mem::take(&mut self.vfos);
        let mut remap = BTreeMap::new();
        for v in old {
            let id = eng.add_vfo(v.settings.clone());
            remap.insert(v.id, id);
            self.vfos.push(UiVfo {
                id,
                settings: v.settings,
            });
        }
        self.selected = self.selected.and_then(|s| remap.get(&s).copied());
        self.statuses.clear();
        self.engine = Some(eng);
        self.history.reset();
        self.last_seq = 0;
        self.front = FrontStatus::default();
        self.sample_rate = rate;
        self.band.set_span(rate);
    }

    /// Stop whatever is running. The radio's control handle holds the USB
    /// device open, so it goes first, then the engine (whose thread owns the
    /// source); only then can the radio be opened again.
    fn stop_source(&mut self) {
        #[cfg(feature = "hackrf")]
        {
            self.hackrf = None;
        }
        self.engine = None;
    }

    /// Start the HackRF with the panel's settings.
    #[cfg(feature = "hackrf")]
    fn start_hackrf(&mut self) {
        self.stop_source();
        self.path = None;
        match decdvb_io::HackRfSource::open(self.radio.settings) {
            Ok(src) => {
                self.hackrf = Some(src.control());
                self.radio.error = None;
                self.note = "Live from the HackRF.".into();
                self.start_engine(Box::new(src));
                self.applied = Some(self.source_key());
            }
            Err(e) => self.radio.error = Some(e.to_string()),
        }
    }

    /// Open a capture, taking rate/centre/format from its name when it says.
    fn open_file(&mut self, path: PathBuf) {
        let g = filename::guess(&path);
        let mut found = Vec::new();
        if let Some(r) = g.rate {
            self.sample_rate = r;
            found.push(format!("rate {}", format::rate(r)));
        }
        if let Some(c) = g.center {
            self.rf_center_mhz = c / 1e6;
            found.push(format!("centre {}", format::freq(c)));
        }
        if let Some(f) = g.format {
            self.format = f;
        }
        self.note = if found.is_empty() {
            "No metadata in the file name: check the sample rate and format.".into()
        } else {
            format!("From the file name: {}.", found.join(", "))
        };
        self.path = Some(path);
        self.restart();
    }

    /// (Re)start the engine on the current source with the current settings,
    /// keeping the VFOs.
    fn restart(&mut self) {
        #[cfg(feature = "hackrf")]
        if self.hackrf.is_some() {
            self.start_hackrf();
            return;
        }
        let Some(path) = self.path.clone() else {
            return;
        };
        self.stop_source();
        match IqFileReader::open(&path, self.format, self.sample_rate, 1 << 16) {
            Ok(reader) => {
                let reader = reader.with_center_freq(self.rf_center());
                self.start_engine(Box::new(reader));
                self.applied = Some(self.source_key());
            }
            Err(e) => self.note = format!("Cannot open {}: {e}", path.display()),
        }
    }

    fn source_key(&self) -> (f64, SampleFormat, usize, bool, bool) {
        (
            self.sample_rate,
            self.format,
            self.opts.fft_size,
            self.opts.realtime,
            self.opts.loop_file,
        )
    }

    fn apply(&mut self, a: Action) {
        match a {
            Action::Select(id) => self.selected = id,
            Action::Update(id, s) => {
                if let Some(v) = self.vfos.iter_mut().find(|v| v.id == id) {
                    v.settings = s.clone();
                }
                if let Some(e) = &self.engine {
                    e.update_vfo(id, s);
                }
            }
            Action::Create(s) => {
                if let Some(e) = &mut self.engine {
                    if s.decoder == DecoderKind::IqRecord {
                        let _ = std::fs::create_dir_all(&s.record_dir);
                    }
                    let id = e.add_vfo(s.clone());
                    self.vfos.push(UiVfo { id, settings: s });
                    self.selected = Some(id);
                    self.names += 1;
                }
            }
            Action::Remove(id) => {
                if let Some(e) = &self.engine {
                    e.remove_vfo(id);
                }
                self.vfos.retain(|v| v.id != id);
                self.statuses.remove(&id);
                if self.selected == Some(id) {
                    self.selected = None;
                }
            }
            Action::Retune(d) => {
                if self.can_retune() {
                    self.set_rf_center(self.rf_center() + d);
                }
            }
            Action::OpenTsViewer(id) => self.ts_viewer.open(id),
            Action::PlayAudio(id, key, p) => {
                if let Some(v) = self.vfos.iter().find(|v| v.id == id) {
                    let mut s = v.settings.clone();
                    s.audio_play = Some(key);
                    s.audio_external = p.is_some();
                    self.apply(Action::Update(id, s));
                    // An external player is launched once the relay is up.
                    self.pending_audio = p.map(|p| (id, key, p, std::time::Instant::now()));
                }
            }
            Action::RecordAudio(id, key) => {
                if let Some(v) = self.vfos.iter().find(|v| v.id == id) {
                    let mut s = v.settings.clone();
                    s.audio_record = key;
                    if key.is_some() {
                        let _ = std::fs::create_dir_all(&s.record_dir);
                    }
                    self.apply(Action::Update(id, s));
                }
            }
            Action::StopAudio(id) => {
                if let Some(v) = self.vfos.iter().find(|v| v.id == id) {
                    let mut s = v.settings.clone();
                    s.audio_play = None;
                    self.apply(Action::Update(id, s));
                }
                self.pending_audio = None;
            }
            Action::Play(id, p) => {
                // The server starts with the VFO's next TS frame; the player
                // is launched once it is up (see `launch_pending_player`).
                if let Some(v) = self.vfos.iter().find(|v| v.id == id)
                    && !v.settings.ts_tcp_on
                {
                    let mut s = v.settings.clone();
                    s.ts_tcp_on = true;
                    self.apply(Action::Update(id, s));
                }
                self.pending_player = Some((id, p, std::time::Instant::now()));
            }
        }
    }

    /// Launch a player on a multicast audio stream once its relay is ready.
    fn launch_pending_audio(&mut self) {
        let Some((id, key, p, since)) = self.pending_audio else {
            return;
        };
        let g = self
            .statuses
            .get(&id)
            .and_then(|st| st.fec.as_ref())
            .and_then(|f| f.gse.as_ref());
        let ready = g.filter(|g| g.audio_playing == Some(key));
        if let Some(e) = ready.and_then(|g| g.audio_error.clone()) {
            self.note = e;
            self.pending_audio = None;
        } else if let Some(t) = ready.and_then(|g| g.audio_target.clone()) {
            let what = match t {
                decdvb_ip::PlayTarget::Sdp(path) => path.display().to_string(),
                decdvb_ip::PlayTarget::Http(addr) => player::http_url(addr),
            };
            self.note = match player::launch(p, &what) {
                Ok(()) => format!("{} opening {what}", p.name()),
                Err(e) => e,
            };
            self.pending_audio = None;
        } else if since.elapsed().as_secs() > 15 {
            self.note = format!("{}: the stream did not start", p.name());
            self.pending_audio = None;
        }
    }

    /// Launch a player asked for with ▶ once its VFO's TCP server is up.
    fn launch_pending_player(&mut self) {
        let Some((id, p, since)) = self.pending_player else {
            return;
        };
        let server = self
            .statuses
            .get(&id)
            .and_then(|st| st.fec.as_ref())
            .and_then(|f| f.ts.as_ref())
            .and_then(|t| t.tcp.as_ref().map(|(a, _)| *a));
        if let Some(addr) = server {
            let url = player::http_url(addr);
            self.note = match player::launch(p, &url) {
                Ok(()) => format!("{} opening {url}", p.name()),
                Err(e) => e,
            };
            self.pending_player = None;
        } else if since.elapsed().as_secs() > 15 {
            self.note = format!(
                "{}: no TS server — is the VFO locked on a TS carrier, and its TCP address free?",
                p.name()
            );
            self.pending_player = None;
        }
    }

    /// The source is a running HackRF, so the view can tune it.
    fn can_retune(&self) -> bool {
        #[cfg(feature = "hackrf")]
        {
            self.path.is_none() && self.hackrf.is_some()
        }
        #[cfg(not(feature = "hackrf"))]
        {
            false
        }
    }

    /// Set the RF frequency of the span's centre: retune a live HackRF (RF −
    /// LO), relabel a file's axis, or pre-set the radio before Start.
    fn set_rf_center(&mut self, rf: f64) {
        #[cfg(feature = "hackrf")]
        if self.path.is_none() {
            let tuned = (rf - self.radio.lnb_lo_mhz * 1e6).clamp(1e6, 6e9).round() as u64;
            self.radio.settings.center_hz = tuned;
            if let Some(c) = &self.hackrf
                && let Err(e) = c.set_center(tuned)
            {
                self.radio.error = Some(e.to_string());
                self.note = e.to_string();
            }
            return;
        }
        self.rf_center_mhz = rf / 1e6;
    }

    /// The SDR++-style frequency bar: the span's centre in big digits.
    fn freq_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let mut rf = self.rf_center();
            if freq_display::show(ui, &mut rf, 0.0, 999_999_999_999.0, 34.0) {
                self.set_rf_center(rf);
            }
            ui.add_space(12.0);
            ui.vertical(|ui| {
                ui.add_space(4.0);
                #[cfg(feature = "hackrf")]
                if self.path.is_none() {
                    let live = self.hackrf.is_some();
                    ui.label(
                        RichText::new(format!(
                            "{} {}  ·  LO {}",
                            if live { "tuned" } else { "radio" },
                            format::freq(self.radio.settings.center_hz as f64),
                            format::freq(self.radio.lnb_lo_mhz * 1e6),
                        ))
                        .weak(),
                    );
                    ui.label(
                        RichText::new(
                            "scroll or click a digit to tune · right-click zeroes from that digit",
                        )
                        .small()
                        .weak(),
                    );
                    return;
                }
                ui.label(RichText::new("centre of the recording (labels the axis)").weak());
                ui.label(
                    RichText::new(
                        "scroll or click a digit to change · right-click zeroes from that digit",
                    )
                    .small()
                    .weak(),
                );
            });
        });
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            let out = prefs::output_dir();
            if ui
                .button("📁 Output folder…")
                .on_hover_text(format!(
                    "Where recordings, symbol files, PCAP and TS files go:\n{}",
                    out.display()
                ))
                .clicked()
                && let Some(dir) = rfd::FileDialog::new().set_directory(&out).pick_folder()
            {
                match prefs::set_output_dir(dir.clone()) {
                    Ok(()) => self.note = format!("Output folder: {}", dir.display()),
                    Err(e) => self.note = format!("Output folder not saved: {e}"),
                }
                // Existing VFOs follow; a recording already open keeps its file.
                let updates: Vec<_> = self
                    .vfos
                    .iter()
                    .map(|v| {
                        let mut s = v.settings.clone();
                        s.record_dir = dir.clone();
                        Action::Update(v.id, s)
                    })
                    .collect();
                for a in updates {
                    self.apply(a);
                }
            }
            if ui.button("📂 Open IQ…").clicked()
                && let Some(p) = rfd::FileDialog::new()
                    .add_filter(
                        "IQ captures",
                        &[
                            "cs8", "s8", "iq8", "cs16", "s16", "cf32", "fc32", "raw", "bin", "iq",
                        ],
                    )
                    .add_filter("All files", &["*"])
                    .pick_file()
            {
                self.open_file(p);
            }
            #[cfg(feature = "hackrf")]
            {
                let live = self.hackrf.is_some();
                let label = if live {
                    "📡 HackRF ●"
                } else {
                    "📡 HackRF"
                };
                if ui
                    .selectable_label(
                        self.radio.open,
                        RichText::new(label).color(if live {
                            egui::Color32::from_rgb(110, 220, 110)
                        } else {
                            ui.visuals().text_color()
                        }),
                    )
                    .on_hover_text("Live input from a HackRF One: frequency, rate, gains, LNB LO.")
                    .clicked()
                {
                    self.radio.open = !self.radio.open;
                }
            }
            #[cfg(not(feature = "hackrf"))]
            ui.add_enabled(false, egui::Button::new("📡 HackRF"))
                .on_disabled_hover_text("Built without the hackrf feature.");
            ui.separator();

            ui.label("Rate");
            let mut msps = self.sample_rate / 1e6;
            if ui
                .add(
                    egui::DragValue::new(&mut msps)
                        .speed(0.01)
                        .range(0.01..=40.0)
                        .max_decimals(6)
                        .suffix(" MS/s"),
                )
                .changed()
            {
                self.sample_rate = msps * 1e6;
            }
            egui::ComboBox::from_id_salt("fmt")
                .width(64.0)
                .selected_text(match self.format {
                    SampleFormat::Cs8 => "cs8",
                    SampleFormat::Cs16 => "cs16",
                    SampleFormat::Cf32 => "cf32",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.format, SampleFormat::Cs8, "cs8");
                    ui.selectable_value(&mut self.format, SampleFormat::Cs16, "cs16");
                    ui.selectable_value(&mut self.format, SampleFormat::Cf32, "cf32");
                });
            egui::ComboBox::from_id_salt("fft")
                .width(70.0)
                .selected_text(format!("FFT {}", self.opts.fft_size))
                .show_ui(ui, |ui| {
                    for n in [1024usize, 2048, 4096, 8192, 16384] {
                        ui.selectable_value(&mut self.opts.fft_size, n, format!("{n}"));
                    }
                });
            ui.checkbox(&mut self.opts.loop_file, "loop");
            ui.checkbox(&mut self.opts.realtime, "real time")
                .on_hover_text("Play files at their sample rate. Off: as fast as the CPU allows.");

            // Rate, format, FFT size, loop and real time need a restart.
            if self.path.is_some()
                && self.applied != Some(self.source_key())
                && ui
                    .button(
                        RichText::new("⟳ Apply")
                            .strong()
                            .color(egui::Color32::from_rgb(255, 200, 80)),
                    )
                    .clicked()
            {
                self.restart();
            }
            ui.separator();

            if self.engine.is_some() {
                let label = if self.paused { "▶ Play" } else { "⏸ Pause" };
                if ui.button(label).clicked() {
                    self.paused = !self.paused;
                    if let Some(e) = &self.engine {
                        e.set_paused(self.paused);
                    }
                }
            }
            if ui.button("⛶ Full span").clicked() {
                self.band.reset_zoom();
            }
            ui.separator();
            if ui
                .checkbox(&mut self.opts.dc_removal, "DC removal")
                .on_hover_text(
                    "Subtract the IQ mean: removes the spike a HackRF leaves at the centre frequency",
                )
                .changed()
                && let Some(e) = &self.engine
            {
                e.set_dc_removal(self.opts.dc_removal);
            }
            ui.checkbox(&mut self.history.auto, "auto levels");
            if !self.history.auto {
                ui.add(
                    egui::DragValue::new(&mut self.history.manual.0)
                        .speed(0.5)
                        .suffix(" dB"),
                );
                ui.add(
                    egui::DragValue::new(&mut self.history.manual.1)
                        .speed(0.5)
                        .suffix(" dB"),
                );
            }
        });
    }

    fn status_bar(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let state = match &self.front.state {
                SourceState::Running if self.engine.is_some() => "playing".to_string(),
                SourceState::Running => "idle".to_string(),
                SourceState::Paused => "paused".to_string(),
                SourceState::Ended => "ended".to_string(),
                SourceState::Failed(e) => format!("failed: {e}"),
            };
            ui.label(if self.front.source.is_empty() {
                "no source".to_string()
            } else {
                self.front.source.clone()
            });
            ui.separator();
            ui.label(state);
            ui.separator();
            let narrow = self.front.carriers.iter().filter(|c| c.narrow).count();
            let carriers = self.front.carriers.len() - narrow;
            ui.label(format!("{carriers} carriers"))
                .on_hover_text("Green brackets: clean carriers. Orange: rough lumps.");
            if narrow > 0 {
                let s = if narrow == 1 { "" } else { "s" };
                ui.label(RichText::new(format!("{narrow} narrow line{s}")).weak())
                    .on_hover_text("CW tones, spurs, comb teeth: the small ticks on the spectrum.");
            }
            ui.separator();
            let secs = self.front.samples as f64 / self.sample_rate.max(1.0);
            ui.label(format!("{secs:.1} s"));
            ui.separator();
            ui.label(format!("front end {:.0}%", self.front.load * 100.0));
            if !self.note.is_empty() {
                ui.separator();
                ui.label(RichText::new(&self.note).weak());
            }
        });
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Files dropped on the window.
        // egui 0.36: a dropped file is a trait object; on native it has a path.
        let dropped = ctx.input(|i| {
            i.raw
                .dropped_files
                .first()
                .map(|f| f.path().to_path_buf())
                .filter(|p| !p.as_os_str().is_empty())
        });
        if let Some(p) = dropped {
            self.open_file(p);
        }

        if let Some(e) = &self.engine {
            self.front = e.front();
            for (seq, row) in e.new_rows(self.last_seq) {
                self.history.push(row);
                self.last_seq = seq;
            }
            for v in &self.vfos {
                if let Some(s) = e.vfo_status(v.id) {
                    self.statuses.insert(v.id, s);
                }
            }
            // Repaint at display rate while running; a row arrives every 40 ms.
            ctx.request_repaint_after(Duration::from_millis(16));
        }

        // Unattended: claim the carriers once the detector has settled — the
        // eight strongest that are not rough lumps, in frequency order.
        if self.automation.claim_carriers && !self.automation.claimed && self.front.row_seq > 40 {
            self.automation.claimed = true;
            let mut picks: Vec<_> = self
                .front
                .carriers
                .iter()
                .copied()
                .filter(|c| c.is_clean())
                .collect();
            picks.sort_by(|a, b| {
                b.snr_db
                    .partial_cmp(&a.snr_db)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            picks.truncate(8);
            picks.sort_by(|a, b| {
                a.center_hz
                    .partial_cmp(&b.center_hz)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            let all = self.front.carriers.clone();
            for c in picks {
                let bw = if c.narrow {
                    c.fit_among(20e3, &all)
                } else {
                    c.vfo_bandwidth_among(&all)
                };
                let s = decdvb_engine::VfoSettings::new(
                    self.next_name(),
                    c.center_hz,
                    bw,
                    self.new_decoder,
                );
                self.apply(Action::Create(s));
            }
            let n = self.automation.select.unwrap_or(1).max(1);
            self.selected = self.vfos.get(n - 1).map(|v| v.id);
            if self.automation.ts_viewer
                && let Some(id) = self.selected
            {
                self.ts_viewer.open(id);
            }
        }
        if self.automation.play_audio
            && !self.automation.audio_started
            && let Some(id) = self.selected
            && let Some(a) = self
                .statuses
                .get(&id)
                .and_then(|st| st.fec.as_ref())
                .and_then(|f| f.gse.as_ref())
                .and_then(|g| g.audio.first())
        {
            self.automation.audio_started = true;
            let key = std::net::SocketAddr::new(a.group, a.port);
            self.apply(Action::PlayAudio(id, key, None));
        }
        self.launch_pending_player();
        self.launch_pending_audio();
        self.automation.handle_screenshot(ctx);
        self.automation.tick(ctx);
        if self.automation.active() {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Panel::top("freqbar").show(ui, |ui| self.freq_bar(ui));
        egui::Panel::top("toolbar").show(ui, |ui| self.toolbar(ui));
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));

        #[cfg(feature = "hackrf")]
        {
            let live = self.hackrf.clone();
            match self.radio.show(ui.ctx(), live.as_ref()) {
                Some(radio::RadioAction::Start) => self.start_hackrf(),
                Some(radio::RadioAction::Stop) => {
                    self.stop_source();
                    self.note = "HackRF stopped.".into();
                }
                None => {}
            }
        }

        let mut actions = Vec::new();
        egui::Panel::right("side")
            .resizable(true)
            .default_size(400.0)
            .min_size(300.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    let inp = side_panel::SideInput {
                        vfos: &self.vfos,
                        statuses: &self.statuses,
                        selected: self.selected,
                        rf_center: self.rf_center(),
                        view_center: (self.band.lo + self.band.hi) / 2.0,
                        view_width: self.band.hi - self.band.lo,
                        next_name: self.next_name(),
                        have_source: self.engine.is_some(),
                    };
                    actions.extend(side_panel::show(ui, &inp, &mut self.new_decoder));
                });
            });

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ui, |ui| {
                let tex = self.ring.update(ui.ctx(), &self.history);
                let inp = BandInput {
                    front: &self.front,
                    rf_center: self.rf_center(),
                    vfos: &self.vfos,
                    statuses: &self.statuses,
                    selected: self.selected,
                    waterfall: tex,
                    levels: self.history.levels(),
                    new_decoder: self.new_decoder,
                    next_name: self.next_name(),
                    can_retune: self.can_retune(),
                };
                actions.extend(self.band.show(ui, &inp));
            });

        // The TS analyser floats over everything, following its VFO.
        if let Some(id) = self.ts_viewer.vfo {
            let name = self
                .vfos
                .iter()
                .find(|v| v.id == id)
                .map(|v| v.settings.name.clone());
            match name {
                Some(name) => {
                    let ts = self
                        .statuses
                        .get(&id)
                        .and_then(|st| st.fec.as_ref())
                        .and_then(|f| f.ts.as_ref());
                    self.ts_viewer.show(ui.ctx(), &name, ts);
                }
                None => self.ts_viewer.vfo = None, // the VFO was deleted
            }
        }

        for a in actions {
            self.apply(a);
        }
    }
}
