// Hide the console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! DecDVB desktop GUI: an SDR++-style wideband waterfall with VFOs, each
//! running a decoder of your choice.

mod automation;
mod band_view;
mod filename;
mod format;
mod side_panel;
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
    let opts = match automation::Options::from_args() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("decdvb-gui: {e}");
            eprintln!(
                "usage: decdvb-gui [capture] [--claim-carriers] [--after SECS] [--screenshot OUT.png]"
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
            let mut app = App {
                automation: automation::Automation::new(&opts),
                ..App::default()
            };
            if let Some(p) = opts.file.clone() {
                app.open_file(p);
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
    note: String,
    automation: automation::Automation,
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
            note: String::new(),
            automation: automation::Automation::new(&automation::Options::default()),
        }
    }
}

impl App {
    fn next_name(&self) -> String {
        format!("VFO {}", self.names)
    }

    fn rf_center(&self) -> f64 {
        self.rf_center_mhz * 1e6
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

    /// (Re)start the engine on the current file with the current settings,
    /// keeping the VFOs.
    fn restart(&mut self) {
        let Some(path) = self.path.clone() else {
            return;
        };
        self.engine = None; // stops the old threads first
        match IqFileReader::open(&path, self.format, self.sample_rate, 1 << 16) {
            Ok(reader) => {
                let reader = reader.with_center_freq(self.rf_center());
                let mut eng = Engine::start(Box::new(reader), self.opts.clone());
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
                self.band.set_span(self.sample_rate);
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
        }
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
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
            ui.add_enabled(false, egui::Button::new("📡 HackRF"))
                .on_disabled_hover_text("Live HackRF input is the next step (M1).");
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
            ui.label("RF centre");
            ui.add(
                egui::DragValue::new(&mut self.rf_center_mhz)
                    .speed(0.01)
                    .max_decimals(6)
                    .suffix(" MHz"),
            )
            .on_hover_text("Only labels the axis; for a Ku LNB, the RF frequency (IF + LO).");
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
            if ui.button("⤢ Full span").clicked() {
                self.band.reset_zoom();
            }
            ui.separator();
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
            ui.label(format!("{} carriers", self.front.carriers.len()));
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

        // Unattended: claim every carrier once the detector has settled.
        if self.automation.claim_carriers && !self.automation.claimed && self.front.row_seq > 40 {
            self.automation.claimed = true;
            for c in self.front.carriers.clone() {
                let bw = if c.narrow {
                    20e3
                } else {
                    c.suggested_vfo_bandwidth()
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
        }
        self.automation.handle_screenshot(ctx);
        self.automation.tick(ctx);
        if self.automation.active() {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Panel::top("toolbar").show(ui, |ui| self.toolbar(ui));
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));

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
                };
                actions.extend(self.band.show(ui, &inp));
            });

        for a in actions {
            self.apply(a);
        }
    }
}
