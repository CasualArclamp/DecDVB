// Hide the console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! DecDVB desktop GUI.
//!
//! M0: open an IQ capture, show its spectrum and constellation, and report
//! level. The demodulator panels (MODCOD/ACM timeline, stream table, PCAP
//! output) arrive with the later milestones — see `docs/DESIGN.md`.

use std::path::PathBuf;

use decdvb_core::{RxConfig, SampleFormat};
use decdvb_engine::{Receiver, Snapshot};
use decdvb_io::{IqFileReader, format_from_path};
use eframe::egui;
use egui_plot::{Line, Plot, PlotPoints, Points};

const FFT: usize = 4096;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 720.0])
            .with_min_inner_size([760.0, 480.0])
            .with_title("DecDVB — DVB-S2/S2X receiver"),
        ..Default::default()
    };
    eframe::run_native(
        "DecDVB",
        options,
        Box::new(|_cc| Ok(Box::new(App::default()))),
    )
}

struct App {
    rx: Option<Receiver>,
    source_name: String,
    snapshot: Snapshot,
    status: String,
    /// Front-end settings the user can edit before opening a capture.
    sample_rate: f64,
    format: SampleFormat,
    running: bool,
}

impl Default for App {
    fn default() -> Self {
        App {
            rx: None,
            source_name: String::new(),
            snapshot: Snapshot::default(),
            status: "Open an IQ capture to begin.".to_owned(),
            sample_rate: 2_000_000.0,
            format: SampleFormat::Cs8,
            running: true,
        }
    }
}

impl App {
    fn open_file(&mut self, path: PathBuf) {
        // The extension wins when it is unambiguous; otherwise keep the user's choice.
        let fmt = format_from_path(&path).unwrap_or(self.format);
        self.format = fmt;

        match IqFileReader::open(&path, fmt, self.sample_rate, FFT * 16) {
            Ok(reader) => {
                self.source_name = path
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let cfg = RxConfig {
                    sample_rate: self.sample_rate,
                    ..Default::default()
                };
                self.rx = Some(Receiver::new(Box::new(reader), cfg, FFT));
                self.snapshot = Snapshot::default();
                self.status = format!("Playing {} as {:?}", self.source_name, fmt);
                self.running = true;
            }
            Err(e) => {
                self.status = format!("Could not open {}: {e}", path.display());
                self.rx = None;
            }
        }
    }

    fn pump(&mut self) {
        let Some(rx) = self.rx.as_mut() else { return };
        if !self.running {
            return;
        }
        match rx.step() {
            Ok(Some(snap)) => self.snapshot = snap,
            Ok(None) => {
                self.status = format!("End of {}", self.source_name);
                self.running = false;
            }
            Err(e) => {
                self.status = format!("Read error: {e}");
                self.running = false;
            }
        }
    }
}

impl eframe::App for App {
    /// Non-drawing work: pull the next block and set the repaint policy.
    ///
    /// Rust/egui note: eframe 0.36 splits the frame into `logic` (has the
    /// `Context`) and `ui` (has a `Ui` that panels attach to).
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump();
        // Keep pulling while a capture is playing.
        if self.running && self.rx.is_some() {
            ctx.request_repaint();
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                if ui.button("Open IQ…").clicked()
                    && let Some(p) = rfd::FileDialog::new()
                        .add_filter(
                            "IQ captures",
                            &[
                                "cs8", "s8", "iq8", "cs16", "s16", "cf32", "fc32", "raw", "bin",
                            ],
                        )
                        .add_filter("All files", &["*"])
                        .pick_file()
                {
                    self.open_file(p);
                }

                ui.separator();
                ui.label("Sample rate");
                let mut msps = self.sample_rate / 1e6;
                if ui
                    .add(
                        egui::DragValue::new(&mut msps)
                            .speed(0.05)
                            .range(0.05..=20.0)
                            .suffix(" MS/s"),
                    )
                    .changed()
                {
                    self.sample_rate = msps * 1e6;
                }

                ui.separator();
                egui::ComboBox::from_label("Format")
                    .selected_text(format!("{:?}", self.format))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.format, SampleFormat::Cs8, "cs8");
                        ui.selectable_value(&mut self.format, SampleFormat::Cs16, "cs16");
                        ui.selectable_value(&mut self.format, SampleFormat::Cf32, "cf32");
                    });

                ui.separator();
                if self.rx.is_some() {
                    let label = if self.running { "Pause" } else { "Play" };
                    if ui.button(label).clicked() {
                        self.running = !self.running;
                    }
                }
            });
        });

        egui::Panel::bottom("status").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(&self.status);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let m = self.snapshot.metrics;
                    ui.label(match m.modcod {
                        Some(mc) => format!("MODCOD {mc}"),
                        None => "no lock".to_owned(),
                    });
                    ui.separator();
                    ui.label(format!("blocks {}", m.frames_total));
                });
            });
        });

        egui::CentralPanel::default().show(ui, |ui| {
            let avail = ui.available_height();
            ui.heading("Spectrum");
            let rate = self.sample_rate;
            let pts: PlotPoints = self
                .snapshot
                .spectrum_db
                .iter()
                .enumerate()
                .map(|(i, &db)| {
                    let hz = (i as f64 / FFT as f64 - 0.5) * rate;
                    [hz / 1e3, db as f64]
                })
                .collect();
            Plot::new("spectrum")
                .height(avail * 0.45)
                .x_axis_label("kHz")
                .y_axis_label("dB")
                .allow_drag(false)
                .show(ui, |p| p.line(Line::new("power", pts)));

            ui.separator();
            ui.heading("Constellation");
            let scatter: PlotPoints = self
                .snapshot
                .scatter
                .iter()
                .map(|s| [s.re as f64, s.im as f64])
                .collect();
            Plot::new("constellation")
                .height(avail * 0.4)
                .data_aspect(1.0)
                .show_grid(false)
                .show(ui, |p| {
                    p.points(Points::new("iq", scatter).radius(1.0));
                });
        });
    }
}
