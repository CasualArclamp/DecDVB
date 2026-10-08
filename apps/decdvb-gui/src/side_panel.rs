//! The side bar: the VFO list, and the selected VFO's settings and results.

use std::collections::BTreeMap;

use decdvb_core::Modulation;
use decdvb_engine::{
    CarrierState, ConstellationGuess, DecoderKind, Identification, LockState, RateSource, Verdict,
    VfoId, VfoStatus,
};
use eframe::egui::{self, Color32, CornerRadius, RichText, Sense, Ui, vec2};
use egui_plot::{Line, Plot, PlotPoints, Points};

use crate::band_view::{Action, UiVfo, badge, default_record_dir, paint_x, vfo_color};
use crate::format;

/// Constellations the generic PSK decoder can be told to use.
const PSK_CHOICES: [Modulation; 5] = [
    Modulation::Bpsk,
    Modulation::Qpsk,
    Modulation::Psk8,
    Modulation::Apsk16,
    Modulation::Apsk32,
];

pub struct SideInput<'a> {
    pub vfos: &'a [UiVfo],
    pub statuses: &'a BTreeMap<VfoId, VfoStatus>,
    pub selected: Option<VfoId>,
    pub rf_center: f64,
    /// Visible span centre, where "New VFO" drops one.
    pub view_center: f64,
    pub view_width: f64,
    pub next_name: String,
    pub have_source: bool,
}

pub fn show(ui: &mut Ui, inp: &SideInput, new_decoder: &mut DecoderKind) -> Vec<Action> {
    let mut actions = Vec::new();

    ui.horizontal(|ui| {
        ui.heading("VFOs");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let b = ui.add_enabled(inp.have_source, egui::Button::new("+ New VFO"));
            if b.on_hover_text("Drop a VFO in the middle of the view. You can also drag across the waterfall, double-click it, or click a detected carrier's bracket.").clicked() {
                let mut s = decdvb_engine::VfoSettings::new(
                    inp.next_name.clone(),
                    inp.view_center,
                    inp.view_width / 25.0,
                    *new_decoder,
                );
                s.record_dir = default_record_dir();
                actions.push(Action::Create(s));
            }
        });
    });
    ui.horizontal(|ui| {
        ui.label("New VFOs run");
        egui::ComboBox::from_id_salt("new_decoder")
            .selected_text(new_decoder.label())
            .show_ui(ui, |ui| {
                for k in DecoderKind::ALL {
                    ui.selectable_value(new_decoder, k, k.label());
                }
            });
    });
    ui.add_space(4.0);

    if inp.vfos.is_empty() {
        ui.label(
            RichText::new("No VFOs yet. Drag across the waterfall to draw one, or click a green carrier bracket to claim it.")
                .weak(),
        );
    }

    // ---- the list
    for v in inp.vfos {
        let sel = Some(v.id) == inp.selected;
        let st = inp.statuses.get(&v.id);
        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 24.0), Sense::click());
        let bg = if sel {
            ui.visuals().selection.bg_fill.gamma_multiply(0.5)
        } else if resp.hovered() {
            ui.visuals().widgets.hovered.weak_bg_fill
        } else {
            Color32::TRANSPARENT
        };
        ui.painter().rect_filled(rect, CornerRadius::same(3), bg);
        let sw = egui::Rect::from_min_size(rect.min + vec2(4.0, 6.0), vec2(12.0, 12.0));
        ui.painter()
            .rect_filled(sw, CornerRadius::same(2), vfo_color(v.id));
        let load = st.map(|s| s.load).unwrap_or(0.0);
        let text = format!(
            "{}  {}  {}",
            v.settings.name,
            v.settings.decoder.short(),
            badge(&v.settings, st)
        );
        ui.painter().text(
            rect.min + vec2(22.0, 12.0),
            egui::Align2::LEFT_CENTER,
            text,
            egui::FontId::proportional(13.0),
            ui.visuals().text_color(),
        );
        let load_col = if load > 0.9 {
            Color32::from_rgb(255, 90, 90)
        } else if load > 0.5 {
            Color32::from_rgb(255, 190, 80)
        } else {
            ui.visuals().weak_text_color()
        };
        // ✕ at the far right deletes the VFO.
        let x_rect =
            egui::Rect::from_center_size(rect.right_center() - vec2(12.0, 0.0), vec2(20.0, 20.0));
        let over_x = resp.hover_pos().is_some_and(|p| x_rect.contains(p));
        if over_x {
            ui.painter().rect_filled(
                x_rect,
                CornerRadius::same(3),
                Color32::from_rgb(170, 50, 50),
            );
        }
        paint_x(
            ui.painter(),
            x_rect.center(),
            4.0,
            if over_x {
                Color32::WHITE
            } else {
                ui.visuals().weak_text_color()
            },
        );
        ui.painter().text(
            x_rect.left_center() - vec2(6.0, 0.0),
            egui::Align2::RIGHT_CENTER,
            format!("CPU {:.0}%", load * 100.0),
            egui::FontId::monospace(11.0),
            load_col,
        );
        if resp.clicked() {
            if resp
                .interact_pointer_pos()
                .is_some_and(|p| x_rect.contains(p))
            {
                actions.push(Action::Remove(v.id));
            } else {
                actions.push(Action::Select(Some(v.id)));
            }
        }
        if over_x {
            resp.on_hover_text("Delete this VFO");
        }
    }

    // ---- the selected VFO
    let Some(v) = inp
        .selected
        .and_then(|id| inp.vfos.iter().find(|v| v.id == id))
    else {
        return actions;
    };
    let st = inp.statuses.get(&v.id).cloned().unwrap_or_default();
    ui.separator();

    let mut s = v.settings.clone();
    egui::Grid::new("vfo_settings")
        .num_columns(2)
        .spacing([8.0, 4.0])
        .show(ui, |ui| {
            ui.label("Name");
            ui.text_edit_singleline(&mut s.name);
            ui.end_row();

            ui.label("Decoder");
            egui::ComboBox::from_id_salt("vfo_decoder")
                .selected_text(s.decoder.label())
                .show_ui(ui, |ui| {
                    for k in DecoderKind::ALL {
                        ui.selectable_value(&mut s.decoder, k, k.label());
                    }
                });
            ui.end_row();

            ui.label("Centre");
            let mut abs_mhz = (inp.rf_center + s.offset_hz) / 1e6;
            if ui
                .add(
                    egui::DragValue::new(&mut abs_mhz)
                        .speed(0.001)
                        .max_decimals(6)
                        .suffix(" MHz"),
                )
                .changed()
            {
                s.offset_hz = abs_mhz * 1e6 - inp.rf_center;
            }
            ui.end_row();

            ui.label("Bandwidth");
            let mut bw_k = s.bandwidth_hz / 1e3;
            if ui
                .add(
                    egui::DragValue::new(&mut bw_k)
                        .speed(1.0)
                        .range(1.0..=100_000.0)
                        .max_decimals(1)
                        .suffix(" kHz"),
                )
                .changed()
            {
                s.bandwidth_hz = bw_k * 1e3;
            }
            ui.end_row();

            if s.decoder == DecoderKind::PskSymbols {
                ui.label("Constellation");
                let txt = s
                    .psk_modulation
                    .map_or("auto (from Identify)", |m| m.name());
                egui::ComboBox::from_id_salt("psk_modulation")
                    .selected_text(txt)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut s.psk_modulation, None, "auto (from Identify)");
                        for m in PSK_CHOICES {
                            ui.selectable_value(&mut s.psk_modulation, Some(m), m.name());
                        }
                    });
                ui.end_row();
            }

            if matches!(
                s.decoder,
                DecoderKind::Dvbs2Ip | DecoderKind::Dvbs2Ts | DecoderKind::PskSymbols
            ) {
                ui.label("Symbol rate");
                ui.horizontal(|ui| {
                    let mut auto = s.symbol_rate.is_none();
                    if ui.checkbox(&mut auto, "blind").changed() {
                        s.symbol_rate = if auto {
                            None
                        } else {
                            Some(st.symbol_rate.unwrap_or(s.bandwidth_hz / 1.35))
                        };
                    }
                    if let Some(rs) = &mut s.symbol_rate {
                        let mut k = *rs / 1e3;
                        if ui
                            .add(
                                egui::DragValue::new(&mut k)
                                    .speed(1.0)
                                    .max_decimals(3)
                                    .suffix(" kS/s"),
                            )
                            .changed()
                        {
                            *rs = k * 1e3;
                        }
                    }
                });
                ui.end_row();
            }
            if matches!(s.decoder, DecoderKind::Dvbs2Ip | DecoderKind::Dvbs2Ts) {
                ui.label("Gold code");
                ui.add(egui::DragValue::new(&mut s.gold_code).range(0..=262_141));
                ui.end_row();
            }

            ui.label("");
            ui.horizontal(|ui| {
                ui.checkbox(&mut s.enabled, "enabled");
                if ui.button("Delete").clicked() {
                    actions.push(Action::Remove(v.id));
                }
            });
            ui.end_row();
        });
    if s != v.settings {
        actions.push(Action::Update(v.id, s.clone()));
    }

    // ---- status line
    ui.add_space(4.0);
    if st.progress > 0.0 && st.progress < 1.0 {
        ui.add(egui::ProgressBar::new(st.progress).text("gathering signal"));
    }
    ui.label(RichText::new(&st.message).strong());
    ui.label(
        RichText::new(format!(
            "{} after ÷{} · CPU {:.0}% · dropped {}",
            format::rate(st.out_rate),
            st.decimation,
            st.load * 100.0,
            st.dropped
        ))
        .weak()
        .small(),
    );

    // ---- results
    // A demodulating VFO leads with its own state and constellation; how it
    // acquired (Identify's view) folds away below.
    let demodulates = matches!(
        v.settings.decoder,
        DecoderKind::Dvbs2Ip | DecoderKind::Dvbs2Ts | DecoderKind::PskSymbols
    );
    if !demodulates && let Some(id) = &st.identification {
        ui.add_space(6.0);
        identification_card(ui, id, inp.rf_center + v.settings.offset_hz);
    }

    if matches!(
        v.settings.decoder,
        DecoderKind::Dvbs2Ip | DecoderKind::Dvbs2Ts
    ) && st.lock.is_some()
    {
        ui.add_space(6.0);
        ui.label(RichText::new("Physical layer").strong());
        egui::Grid::new("pl").num_columns(2).show(ui, |ui| {
            ui.label("Lock");
            let (txt, col) = match st.lock {
                Some(LockState::Locked) => ("locked", Color32::from_rgb(110, 220, 110)),
                Some(LockState::Found) => ("confirming", Color32::from_rgb(240, 200, 80)),
                _ => ("searching", Color32::from_rgb(230, 110, 110)),
            };
            ui.colored_label(col, txt);
            ui.end_row();
            ui.label("Frames");
            ui.label(format!("{} ({} lock losses)", st.frames, st.lock_losses));
            ui.end_row();
            if let Some(c) = &st.carrier {
                carrier_rows(ui, c);
            }
            if let Some(rs) = st.symbol_rate {
                ui.label("Symbol rate");
                ui.label(format::rate(rs));
                ui.end_row();
            }
        });
    }

    if v.settings.decoder == DecoderKind::PskSymbols
        && let Some(c) = &st.carrier
    {
        ui.add_space(6.0);
        ui.label(RichText::new("Demodulator").strong());
        egui::Grid::new("psk").num_columns(2).show(ui, |ui| {
            carrier_rows(ui, c);
            if let Some(rs) = st.symbol_rate {
                ui.label("Symbol rate");
                ui.label(format::rate(rs));
                ui.end_row();
            }
            if let Some((_, n)) = &st.recording {
                ui.label("Symbols");
                ui.label(format!("{n} written ({:.1} MB)", *n as f64 / 1e6));
                ui.end_row();
            }
        });
        psk_note(ui);
    }

    // ---- plots
    ui.add_space(6.0);
    let side = ui.available_width().min(260.0);
    ui.label(RichText::new("Constellation").strong());
    constellation(ui, &st.scatter, side);
    if !st.spectrum_db.is_empty() {
        ui.label(RichText::new("VFO spectrum").strong());
        vfo_spectrum(ui, &st.spectrum_db, st.out_rate);
    }
    if let Some((path, _)) = &st.recording {
        ui.add_space(4.0);
        let what = if v.settings.decoder == DecoderKind::PskSymbols {
            "Symbols to"
        } else {
            "Recording to"
        };
        ui.label(RichText::new(format!("{what} {}", path.display())).small());
    }

    // MODCOD counts and acquisition details, after the plots.
    if matches!(
        v.settings.decoder,
        DecoderKind::Dvbs2Ip | DecoderKind::Dvbs2Ts
    ) {
        modcod_table(ui, &st.modcods);
    }
    if demodulates && let Some(id) = &st.identification {
        ui.add_space(6.0);
        egui::CollapsingHeader::new("Acquisition (Identify)")
            .id_salt(("acq", v.id))
            .show(ui, |ui| {
                identification_card(ui, id, inp.rf_center + v.settings.offset_hz);
            });
    }

    actions
}

/// Carrier loop rows for a two-column grid.
fn carrier_rows(ui: &mut Ui, c: &CarrierState) {
    ui.label("Carrier");
    if c.locked {
        ui.colored_label(
            Color32::from_rgb(110, 220, 110),
            format!("{} locked · MER {:.1} dB", c.modulation.name(), c.mer_db),
        );
    } else {
        ui.colored_label(
            Color32::from_rgb(230, 150, 90),
            format!("{} not locked", c.modulation.name()),
        );
    }
    ui.end_row();
    ui.label("Residual offset");
    ui.label(format!("{:+.0} Hz", c.offset_hz));
    ui.end_row();
}

fn psk_note(ui: &mut Ui) {
    ui.label(
        RichText::new(
            "One byte per symbol: its DVB-S2 bit label (BPSK: 0 = +1). Without a \
             preamble the phase is ambiguous by the constellation's symmetry, so the \
             labels may be a fixed rotation of the sent ones.",
        )
        .weak()
        .small(),
    );
}

fn identification_card(ui: &mut Ui, id: &Identification, abs_center: f64) {
    let (headline, col) = match &id.verdict {
        Verdict::NoSignal => ("No signal".to_string(), Color32::GRAY),
        Verdict::Carrier => (
            "Narrow carrier".to_string(),
            Color32::from_rgb(200, 200, 120),
        ),
        Verdict::DvbS2(d) => {
            let kind = if d.uses_reserved_modcods() {
                "DVB-S2X"
            } else {
                "DVB-S2"
            };
            let mode = if d.variable_coding() {
                "ACM / VCM"
            } else {
                "CCM"
            };
            (format!("{kind} — {mode}"), Color32::from_rgb(110, 220, 110))
        }
        Verdict::NotDvbS2 { hint } => (hint.clone(), Color32::from_rgb(240, 190, 90)),
    };
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new(headline).size(16.0).color(col).strong());
        egui::Grid::new("ident")
            .num_columns(2)
            .spacing([8.0, 2.0])
            .show(ui, |ui| {
                if let Some(rs) = id.symbol_rate {
                    ui.label("Symbol rate");
                    let how = match id.symbol_rate_source {
                        Some(RateSource::Timing) => "timing loop",
                        Some(RateSource::Cyclic) => "cyclic line",
                        Some(RateSource::Enbw) => "spectrum width, approx.",
                        None => "",
                    };
                    ui.label(format!("{}  ({how})", format::rate(rs)));
                    ui.end_row();
                }
                if let Some(r) = id.roll_off {
                    ui.label("Roll-off");
                    ui.label(format!("{:.2}", r.as_f64()));
                    ui.end_row();
                }
                // For DVB-S2 the PLHEADERs say exactly which modulations are
                // in use; the blind estimate only matters when they cannot.
                // (On an ACM carrier the blind classifier sees a mixture of
                // constellations and would report something meaningless.)
                if let Verdict::DvbS2(d) = &id.verdict {
                    let mut mods: Vec<&str> = d
                        .modcods
                        .keys()
                        .filter_map(|&m| decdvb_core::s2_modcod(m, decdvb_core::FecFrame::Normal))
                        .map(|mc| mc.modulation.name())
                        .collect();
                    mods.dedup();
                    if !mods.is_empty() {
                        ui.label("Modulation");
                        ui.label(format!("{} (from the PLHEADERs)", mods.join(" / ")));
                        ui.end_row();
                    }
                } else if let Some(c) = id.constellation {
                    ui.label("Constellation");
                    let txt = match c {
                        ConstellationGuess::Unclear => "unclear".into(),
                        _ => format!("{} (estimate)", c.label()),
                    };
                    ui.label(txt);
                    ui.end_row();
                }
                ui.label("Centre");
                ui.label(format!(
                    "{} ({:+.1} kHz in VFO)",
                    format::freq(abs_center + id.center_offset_hz),
                    id.center_offset_hz / 1e3
                ));
                ui.end_row();
                ui.label("Occupied");
                ui.label(format::freq(id.occupied_bw_hz));
                ui.end_row();
                ui.label("S/N (spectrum)");
                ui.label(format!("{:.1} dB", id.snr_db));
                ui.end_row();
                if let (Some(mer), Some(coh)) = (id.mer_db, id.coherence) {
                    ui.label("Carrier");
                    if id.carrier_locked {
                        ui.colored_label(
                            Color32::from_rgb(110, 220, 110),
                            format!("locked · MER {mer:.1} dB"),
                        );
                    } else {
                        ui.colored_label(
                            Color32::from_rgb(230, 150, 90),
                            format!("not locked (coherence {coh:.2})"),
                        );
                    }
                    ui.end_row();
                }
                if let Some(f) = id.carrier_offset_hz {
                    ui.label("Residual offset");
                    ui.label(format!("{f:+.0} Hz"));
                    ui.end_row();
                }
                if let Verdict::DvbS2(d) = &id.verdict {
                    ui.label("Frames");
                    ui.label(format!(
                        "{} confirmed of {} headers; {} with pilots, {} short, {} dummy",
                        d.headers_confirmed,
                        d.headers_seen,
                        d.with_pilots,
                        d.short_frames,
                        d.dummy_frames
                    ));
                    ui.end_row();
                }
            });
        if let Verdict::DvbS2(d) = &id.verdict {
            let counts: BTreeMap<u8, u64> =
                d.modcods.iter().map(|(&k, &v)| (k, v as u64)).collect();
            modcod_table(ui, &counts);
        }
    });
}

fn modcod_table(ui: &mut Ui, counts: &BTreeMap<u8, u64>) {
    if counts.is_empty() {
        return;
    }
    let total: u64 = counts.values().sum();
    egui::Grid::new(ui.next_auto_id())
        .num_columns(3)
        .striped(true)
        .show(ui, |ui| {
            ui.label(RichText::new("MODCOD").small().strong());
            ui.label(RichText::new("frames").small().strong());
            ui.label(RichText::new("share").small().strong());
            ui.end_row();
            for (&m, &n) in counts {
                let name = if m == 0 {
                    "dummy".to_string()
                } else {
                    decdvb_core::s2_modcod(m, decdvb_core::FecFrame::Normal)
                        .map(|mc| format!("{m:2} {mc}"))
                        .unwrap_or_else(|| format!("{m:2} (S2X/reserved)"))
                };
                ui.label(RichText::new(name).monospace());
                ui.label(n.to_string());
                ui.add(egui::ProgressBar::new(n as f32 / total.max(1) as f32).desired_width(80.0));
                ui.end_row();
            }
        });
}

fn constellation(ui: &mut Ui, pts: &[decdvb_core::Iq], side: f32) {
    let rms = (pts.iter().map(|s| s.norm_sqr()).sum::<f32>() / pts.len().max(1) as f32)
        .sqrt()
        .max(1e-9);
    let p: PlotPoints = pts
        .iter()
        .map(|s| [(s.re / rms) as f64, (s.im / rms) as f64])
        .collect();
    Plot::new("constellation")
        .width(side)
        .height(side)
        .data_aspect(1.0)
        .include_x(-1.8)
        .include_x(1.8)
        .include_y(-1.8)
        .include_y(1.8)
        .show_axes(false)
        .show_grid(false)
        .allow_drag(false)
        .allow_zoom(false)
        .allow_scroll(false)
        .allow_boxed_zoom(false)
        .show(ui, |pl| {
            pl.points(
                Points::new("symbols", p)
                    .radius(1.2)
                    .color(Color32::from_rgb(120, 200, 255)),
            );
        });
}

fn vfo_spectrum(ui: &mut Ui, db: &[f32], rate: f64) {
    let n = db.len().max(1);
    let p: PlotPoints = db
        .iter()
        .enumerate()
        .map(|(k, &d)| [((k as f64 / n as f64) - 0.5) * rate / 1e3, d as f64])
        .collect();
    Plot::new("vfo_spectrum")
        .height(120.0)
        .x_axis_label("kHz")
        .allow_drag(false)
        .allow_zoom(false)
        .allow_scroll(false)
        .show(ui, |pl| pl.line(Line::new("power", p)));
}
