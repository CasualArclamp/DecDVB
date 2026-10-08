//! The side bar: the VFO list, and the selected VFO's settings and results.

use std::collections::BTreeMap;

use decdvb_core::Modulation;
use decdvb_engine::{
    CarrierState, ConstellationGuess, DecoderKind, FecStats, GseView, Identification, LockState,
    RateSource, TsView, Verdict, VfoId, VfoStatus,
};
use decdvb_gse::{Source, Variant};
use eframe::egui::{self, Color32, CornerRadius, RichText, Sense, Ui, vec2};
use egui_plot::{Line, Plot, PlotPoints, Points};

use crate::band_view::{Action, UiVfo, badge, default_record_dir, paint_x, vfo_color};
use crate::format;
use crate::player::{self, Player};

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

                // Symbols go to a file only while this is on; until then the
                // decoder just shows the locked constellation.
                ui.label("Symbols");
                ui.horizontal(|ui| {
                    let (label, tip) = if s.record {
                        ("⏹ Stop", "Close the .bin file")
                    } else {
                        (
                            "● Record",
                            "Write the hard-decided symbols to a .bin file, one byte each",
                        )
                    };
                    let b = egui::Button::new(label).selected(s.record);
                    if ui.add(b).on_hover_text(tip).clicked() {
                        s.record = !s.record;
                    }
                    if s.record && !st.recording_active {
                        ui.label(RichText::new("starts once locked").weak().small());
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

            if s.decoder == DecoderKind::Dvbs2Ts {
                let ts = st.fec.as_ref().and_then(|f| f.ts.as_ref());
                ui.label("TS file");
                ui.horizontal(|ui| {
                    let (label, tip) = if s.record {
                        ("⏹ Stop", "Close the .ts file")
                    } else {
                        ("● Record", "Write the transport stream to a .ts file")
                    };
                    if ui
                        .add(egui::Button::new(label).selected(s.record))
                        .on_hover_text(tip)
                        .clicked()
                    {
                        s.record = !s.record;
                    }
                });
                ui.end_row();

                ui.label("UDP");
                ui.horizontal(|ui| {
                    ui.checkbox(&mut s.ts_udp_on, "");
                    address_field(ui, &mut s.ts_udp);
                })
                .response
                .on_hover_text(udp_hint(&s.ts_udp));
                ui.end_row();

                ui.label("TCP / HTTP");
                ui.horizontal(|ui| {
                    ui.checkbox(&mut s.ts_tcp_on, "");
                    address_field(ui, &mut s.ts_tcp);
                })
                .response
                .on_hover_text(
                    "A server players connect to: http://<address>/ in VLC or PotPlayer \
                     (VLC also tcp://<address>). 127.0.0.1 is this machine only; \
                     0.0.0.0 lets other machines on the network connect.",
                );
                ui.end_row();

                ui.label("Play");
                ui.horizontal(|ui| {
                    for p in [Player::Vlc, Player::PotPlayer] {
                        if ui
                            .button(format!("▶ {}", p.name()))
                            .on_hover_text(
                                "Start the TCP server if needed and open the stream in the player",
                            )
                            .clicked()
                        {
                            actions.push(Action::Play(v.id, p));
                        }
                    }
                    if let Some((addr, _)) = ts.and_then(|t| t.tcp.as_ref()) {
                        ui.label(RichText::new(player::http_url(*addr)).small().weak());
                    }
                });
                ui.end_row();
            }

            if s.decoder == DecoderKind::Dvbs2Ip {
                ui.label("GSE");
                let txt = s.gse_variant.map_or("auto (from the data)", |v| v.label());
                egui::ComboBox::from_id_salt("gse_variant")
                    .selected_text(txt)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut s.gse_variant, None, "auto (from the data)");
                        for v in Variant::ALL {
                            ui.selectable_value(&mut s.gse_variant, Some(v), v.label());
                        }
                    });
                ui.end_row();

                ui.label("PCAP");
                ui.horizontal(|ui| {
                    let (label, tip) = if s.record {
                        ("⏹ Stop", "Close the PCAP file")
                    } else {
                        (
                            "● Record",
                            "Write the IP packets to a PCAP file (raw IP, Wireshark reads it)",
                        )
                    };
                    let b = egui::Button::new(label).selected(s.record);
                    if ui.add(b).on_hover_text(tip).clicked() {
                        s.record = !s.record;
                    }
                    let active = st
                        .fec
                        .as_ref()
                        .and_then(|f| f.gse.as_ref())
                        .is_some_and(|g| g.pcap_active);
                    if s.record && !active {
                        ui.label(
                            RichText::new("starts with the first GSE frame")
                                .weak()
                                .small(),
                        );
                    }
                });
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
    // Radio stations found in the IP come first: they are what one is
    // usually after on a carrier that has them.
    if let Some(g) = st.fec.as_ref().and_then(|f| f.gse.as_ref()) {
        audio_card(ui, g, v.id, v.settings.audio_external, &mut actions);
    }
    // A demodulating VFO leads with its own state and constellation; how it
    // acquired (Identify's view) folds away below.
    let demodulates = matches!(
        v.settings.decoder,
        DecoderKind::Dvbs2Ip | DecoderKind::Dvbs2Ts | DecoderKind::PskSymbols
    );
    if !demodulates && let Some(id) = &st.identification {
        ui.add_space(6.0);
        identification_card(
            ui,
            id,
            inp.rf_center + v.settings.offset_hz,
            st.carrier.as_ref(),
        );
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
        if let Some(f) = &st.fec {
            fec_card(ui, f);
            if let Some(g) = &f.gse {
                gse_card(ui, g);
            }
            if let Some(t) = &f.ts {
                ts_card(ui, t, v.id, &mut actions);
            }
        }
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
                ui.label("Written");
                let txt = format!("{n} symbols ({:.1} MB)", *n as f64 / 1e6);
                if st.recording_active {
                    ui.colored_label(Color32::from_rgb(230, 90, 90), format!("● {txt}"));
                } else {
                    ui.label(format!("{txt}, stopped"));
                }
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
        let what = match (v.settings.decoder, st.recording_active) {
            (DecoderKind::PskSymbols, true) => "Symbols to",
            (DecoderKind::PskSymbols, false) => "Last file:",
            _ => "Recording to",
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
                identification_card(ui, id, inp.rf_center + v.settings.offset_hz, None);
            });
    }

    actions
}

/// What FEC made of a DVB-S2 VFO's frames, and the stream the BBHEADERs
/// describe.
fn fec_card(ui: &mut Ui, f: &FecStats) {
    ui.add_space(6.0);
    ui.label(RichText::new("FEC").strong());
    egui::Grid::new("fec").num_columns(2).show(ui, |ui| {
        ui.label("BBFRAMEs");
        let pct = 100.0 * f.ok as f64 / f.frames.max(1) as f64;
        let col = if f.frames == 0 {
            ui.visuals().weak_text_color()
        } else if f.ok == f.frames {
            Color32::from_rgb(110, 220, 110)
        } else if f.ok > 0 {
            Color32::from_rgb(240, 200, 80)
        } else {
            Color32::from_rgb(230, 110, 110)
        };
        ui.colored_label(col, format!("{} of {} good ({pct:.1} %)", f.ok, f.frames));
        ui.end_row();
        if f.bch_failed + f.crc_failed > 0 {
            ui.label("Failed");
            ui.label(format!(
                "{} LDPC/BCH, {} header CRC",
                f.bch_failed, f.crc_failed
            ));
            ui.end_row();
        }
        if f.frames > 0 {
            ui.label("LDPC");
            ui.label(format!(
                "{:.1} iterations avg · BCH fixed {} bits",
                f.iterations as f64 / f.frames as f64,
                f.bch_corrected
            ));
            ui.end_row();
        }
        if let Some(e) = f.es_n0_db {
            ui.label("Es/N0");
            ui.label(format!("{e:.1} dB (pilots and headers)"));
            ui.end_row();
        }
        if f.payload_bps > 0.0 {
            ui.label("Payload");
            ui.label(format::bitrate(f.payload_bps));
            ui.end_row();
        }
        if let Some(h) = &f.last_header {
            ui.label("Stream");
            let streams = if h.single_stream {
                "single stream".to_string()
            } else {
                let isis: Vec<String> = f.streams.keys().map(|i| i.to_string()).collect();
                format!("multistream, ISI {}", isis.join(", "))
            };
            ui.label(format!(
                "{} · {} · {}{}",
                h.format.label(),
                if h.ccm { "CCM" } else { "ACM/VCM" },
                streams,
                if h.high_efficiency { " · HEM" } else { "" }
            ));
            ui.end_row();
            if h.upl > 0 {
                ui.label("Packets");
                ui.label(format!("{} bytes, sync 0x{:02X}", h.upl / 8, h.sync));
                ui.end_row();
            }
            if let Some(r) = h.roll_off {
                ui.label("Roll-off");
                ui.label(format!("{:.2} (BBHEADER)", r.as_f64()));
                ui.end_row();
            }
        }
        if f.dropped > 0 || f.load > 0.5 {
            ui.label("FEC load");
            ui.label(format!(
                "{:.0} % · {} frames dropped",
                f.load * 100.0,
                f.dropped
            ));
            ui.end_row();
        }
    });
}

/// Multicast audio found in the IP, by address: name (from SAP), codec and
/// rate, with play (in the app, or in VLC/PotPlayer) and record buttons.
/// `external` is whether the stream asked for plays in an external player.
fn audio_card(ui: &mut Ui, g: &GseView, id: VfoId, external: bool, actions: &mut Vec<Action>) {
    if g.audio.is_empty() {
        return;
    }
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("Multicast audio ({})", g.audio.len())).strong());
        ui.with_layout(
            egui::Layout::right_to_left(egui::Align::Center),
            volume_control,
        );
    });
    for a in &g.audio {
        let key = std::net::SocketAddr::new(a.group, a.port);
        let playing = g.audio_playing == Some(key);
        let recording = g.audio_recording == Some(key);
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                if recording {
                    ui.colored_label(Color32::from_rgb(230, 70, 70), "●")
                        .on_hover_text("Recording");
                }
                let name = RichText::new(a.name()).strong();
                ui.label(if playing {
                    name.color(Color32::from_rgb(110, 220, 110))
                } else {
                    name
                });
                ui.label(
                    RichText::new(format!(
                        "{}:{} · {} · {}{}",
                        a.group,
                        a.port,
                        a.codec.label(),
                        format::bitrate(a.rate_bps),
                        if a.rtp {
                            format!(" · RTP PT {}", a.pt.unwrap_or(0))
                        } else {
                            " · raw UDP".into()
                        }
                    ))
                    .small()
                    .weak(),
                );
            });
            let app = g.audio_app.as_ref().filter(|_| playing && !external);
            ui.horizontal(|ui| {
                if !playing {
                    if ui.button("▶ Play").on_hover_text("Play it here").clicked() {
                        actions.push(Action::PlayAudio(id, key, None));
                    }
                } else {
                    if let Some(h) = app {
                        let (label, tip) = if h.paused() {
                            ("▶ Resume", "Carry on, live")
                        } else {
                            ("⏸ Pause", "Pause (resuming plays live)")
                        };
                        if ui.button(label).on_hover_text(tip).clicked() {
                            h.set_paused(!h.paused());
                        }
                    }
                    if ui.button("⏹ Stop").clicked() {
                        actions.push(Action::StopAudio(id));
                    }
                }
                let rec = if recording {
                    ui.button(
                        RichText::new("⏹ Stop recording").color(Color32::from_rgb(230, 90, 90)),
                    )
                } else {
                    ui.button("⏺ Record").on_hover_text(
                        "Save the stream to a file in the output folder, as broadcast",
                    )
                };
                if rec.clicked() {
                    actions.push(Action::RecordAudio(id, (!recording).then_some(key)));
                }
                if let Some(h) = app {
                    level_meter(ui, h.levels());
                }
                ui.menu_button("…", |ui| {
                    for p in [Player::Vlc, Player::PotPlayer] {
                        if ui.button(format!("▶ Open in {}", p.name())).clicked() {
                            actions.push(Action::PlayAudio(id, key, Some(p)));
                            ui.close();
                        }
                    }
                })
                .response
                .on_hover_text("Open in an external player");
            });
            if playing {
                let state = if let Some(e) = &g.audio_error {
                    e.clone()
                } else if let Some(h) = app {
                    playback_state(&h.status())
                } else if external && g.audio_target.is_some() {
                    format!("relaying to the player · {} packets", g.audio_forwarded)
                } else {
                    "waiting for the stream…".into()
                };
                ui.label(RichText::new(state).small());
            }
            if recording {
                let text = match (&g.audio_record_error, &g.audio_record_file) {
                    (Some(e), _) => e.clone(),
                    (None, Some((path, n))) => format!(
                        "recording {} · {}",
                        path.file_name()
                            .map(|f| f.to_string_lossy())
                            .unwrap_or_default(),
                        format::bytes(*n)
                    ),
                    (None, None) => "recording starts with the next frame…".into(),
                };
                ui.label(
                    RichText::new(text)
                        .small()
                        .color(Color32::from_rgb(230, 120, 120)),
                );
            }
            if let Some(info) = a.sdp.as_ref().and_then(|s| s.info.clone()) {
                ui.label(RichText::new(info).small().weak());
            }
        });
    }
    if g.audio_recording.is_none()
        && let Some((path, n)) = &g.audio_record_file
    {
        ui.label(
            RichText::new(format!("Saved {} ({})", path.display(), format::bytes(*n)))
                .small()
                .weak(),
        );
    }
    if g.sap_packets > 0 {
        ui.label(
            RichText::new(format!("{} SAP announcements heard", g.sap_packets))
                .small()
                .weak(),
        );
    }
}

/// The app-wide mute button and volume slider (laid out right to left).
fn volume_control(ui: &mut Ui) {
    let mut v = decdvb_audio::volume();
    ui.spacing_mut().slider_width = 90.0;
    let r = ui
        .add(egui::Slider::new(&mut v, 0.0..=1.0).show_value(false))
        .on_hover_text(format!("Volume {:.0} %", v * 100.0));
    if r.changed() {
        decdvb_audio::set_volume(v);
        if decdvb_audio::muted() {
            decdvb_audio::set_muted(false);
        }
    }
    if r.drag_stopped() || (r.changed() && !r.dragged()) {
        crate::prefs::save_audio();
    }
    let muted = decdvb_audio::muted();
    let (icon, tip) = if muted {
        ("🔇", "Unmute")
    } else {
        ("🔊", "Mute")
    };
    if ui.button(icon).on_hover_text(tip).clicked() {
        decdvb_audio::set_muted(!muted);
        crate::prefs::save_audio();
    }
}

/// Two thin bars, left over right: peak level on a 60 dB scale.
fn level_meter(ui: &mut Ui, levels: [f32; 2]) {
    let (rect, _) = ui.allocate_exact_size(vec2(70.0, 12.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(2), Color32::from_gray(40));
    for (i, &l) in levels.iter().enumerate() {
        let db = 20.0 * l.max(1e-6).log10();
        let frac = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
        let top = rect.top() + 1.0 + i as f32 * 5.5;
        let bar = egui::Rect::from_min_size(
            egui::pos2(rect.left() + 1.0, top),
            vec2((rect.width() - 2.0) * frac, 4.5),
        );
        let col = if db > -3.0 {
            Color32::from_rgb(230, 80, 70)
        } else if db > -12.0 {
            Color32::from_rgb(230, 200, 80)
        } else {
            Color32::from_rgb(100, 210, 110)
        };
        p.rect_filled(bar, CornerRadius::same(1), col);
    }
}

/// "playing · MPEG-1 Layer II · 128 kbit/s · 48 kHz · stereo · buffer 0.4 s".
fn playback_state(s: &decdvb_audio::Status) -> String {
    use decdvb_audio::PlayState;
    if s.state == PlayState::Failed {
        return s.error.clone().unwrap_or_else(|| "failed".into());
    }
    let mut parts = vec![
        match s.state {
            PlayState::Starting => "starting…",
            PlayState::Buffering => "buffering…",
            PlayState::Playing => "playing",
            PlayState::Paused => "paused",
            PlayState::Failed => "failed",
        }
        .to_string(),
    ];
    match (&s.codec, &s.error) {
        (Some(c), _) => parts.push(c.clone()),
        (None, Some(e)) => parts.push(e.clone()),
        (None, None) => parts.push(s.carriage.clone()),
    }
    if s.state == PlayState::Playing || s.state == PlayState::Buffering {
        parts.push(format!("buffer {:.1} s", s.buffer_ms as f32 / 1000.0));
    }
    if s.underruns > 0 {
        parts.push(format!("{} dropouts", s.underruns));
    }
    if s.lost_packets > 0 {
        parts.push(format!("{} packets lost", s.lost_packets));
    }
    if s.decode_errors > 0 {
        parts.push(format!("{} bad frames", s.decode_errors));
    }
    parts.join(" · ")
}

/// A "host:port" text box, red while it does not parse.
fn address_field(ui: &mut Ui, text: &mut String) {
    let ok = text.trim().parse::<std::net::SocketAddr>().is_ok();
    let mut edit = egui::TextEdit::singleline(text).desired_width(150.0);
    if !ok {
        edit = edit.text_color(Color32::from_rgb(230, 110, 110));
    }
    ui.add(edit);
}

/// How to open a UDP stream sent to `addr` in the players.
fn udp_hint(addr: &str) -> String {
    let port = addr.rsplit(':').next().unwrap_or("1234");
    format!(
        "Sends the TS to this address, 7 packets per datagram.\n\
         VLC: udp://@:{port}   PotPlayer: udp://127.0.0.1:{port}\n\
         A multicast address (239.x.x.x) reaches several players."
    )
}

/// The transport stream: what it carries, its health, and where it goes.
fn ts_card(ui: &mut Ui, t: &TsView, id: VfoId, actions: &mut Vec<Action>) {
    ui.add_space(6.0);
    ui.label(RichText::new("MPEG-TS").strong());
    egui::Grid::new("ts").num_columns(2).show(ui, |ui| {
        ui.label("Packets");
        ui.label(format!("{} · {}", t.packets, format::bitrate(t.ts_bps)));
        ui.end_row();
        ui.label("Errors");
        let col = if t.crc_errors + t.cc_errors == 0 {
            Color32::from_rgb(110, 220, 110)
        } else {
            Color32::from_rgb(240, 200, 80)
        };
        ui.colored_label(
            col,
            format!(
                "{} CRC · {} continuity · {} resyncs",
                t.crc_errors, t.cc_errors, t.resyncs
            ),
        );
        ui.end_row();
        if t.nulls_reinserted > 0 {
            ui.label("Null packets");
            ui.label(format!("{} re-inserted (NPD)", t.nulls_reinserted));
            ui.end_row();
        }
        if t.issy {
            ui.label("ISSY");
            ui.colored_label(
                Color32::from_rgb(240, 200, 80),
                "in use — not read yet; packets are not extracted",
            );
            ui.end_row();
        }
        if let Some((path, n)) = &t.file {
            ui.label("File");
            if t.file_active {
                ui.colored_label(Color32::from_rgb(230, 90, 90), format!("● {n} packets"));
            } else {
                ui.label(format!("stopped, {n} packets"));
            }
            ui.end_row();
            ui.label("");
            ui.label(RichText::new(path.display().to_string()).small());
            ui.end_row();
        }
        if let Some((addr, n)) = &t.udp {
            ui.label("UDP");
            ui.label(format!("→ {addr} · {n} datagrams"));
            ui.end_row();
        }
        if let Some((addr, clients)) = &t.tcp {
            ui.label("TCP");
            let who = if clients.is_empty() {
                "no players connected".to_string()
            } else {
                let v: Vec<String> = clients.iter().map(|c| c.to_string()).collect();
                format!("{} connected: {}", clients.len(), v.join(", "))
            };
            ui.label(format!("{} · {who}", player::http_url(*addr)));
            ui.end_row();
        }
        if let Some(e) = &t.error {
            ui.label("Output");
            ui.colored_label(Color32::from_rgb(230, 110, 110), e);
            ui.end_row();
        }
    });
    let r = &t.report;
    let scrambled = |pid: u16| r.pids.iter().any(|p| p.pid == pid && p.stats.scrambled > 0);
    if !r.programmes.is_empty() {
        ui.add_space(4.0);
        ui.label(RichText::new("Services").small().strong());
        for p in &r.programmes {
            let name = p.name.as_deref().unwrap_or("(no name)");
            let encrypted = p.streams.iter().any(|e| scrambled(e.pid));
            ui.label(
                RichText::new(format!(
                    "{} {name}{}",
                    p.number,
                    if encrypted { "  🔒" } else { "" }
                ))
                .strong(),
            );
            if let Some(e) = &p.now {
                ui.label(RichText::new(format!("    now: {}", e.name)).small());
            }
        }
    }
    ui.add_space(4.0);
    if ui
        .button(format!("🔍 TS analyser ({} PIDs)", r.pids.len()))
        .on_hover_text("Every PID with its type, service and bitrate; services, network and tables")
        .clicked()
    {
        actions.push(Action::OpenTsViewer(id));
    }
}

/// IP out of GSE: where it comes from, how much, who is talking, and the
/// PCAP.
fn gse_card(ui: &mut Ui, g: &GseView) {
    ui.add_space(6.0);
    ui.label(RichText::new("IP").strong());
    egui::Grid::new("gse").num_columns(2).show(ui, |ui| {
        ui.label("Source");
        let txt = match (g.source, &g.mpe) {
            (Some(Source::Gse(v)), _) => format!("GSE, {}", v.label()),
            (Some(Source::Blind), _) => "blind IPv4 search (no GSE variant fits)".into(),
            (None, Some(m)) => {
                let pids: Vec<String> = m.pids.keys().map(|p| format!("{p:#06x}")).collect();
                format!("MPE on PID {} · {} datagrams", pids.join(", "), m.datagrams)
            }
            (None, None) => "no IP found yet".into(),
        };
        ui.label(txt);
        ui.end_row();
        ui.label("IP packets");
        ui.label(format!(
            "{} ({:.1} MB) · IPv4 {} · IPv6 {}",
            g.packets,
            g.bytes as f64 / 1e6,
            g.ipv4,
            g.ipv6
        ));
        ui.end_row();
        if g.ip_bps > 0.0 {
            ui.label("IP rate");
            ui.label(format::bitrate(g.ip_bps));
            ui.end_row();
        }
        if !g.protocols.is_empty() {
            ui.label("Protocols");
            let names: Vec<String> = g
                .protocols
                .iter()
                .rev()
                .map(|(&p, &n)| {
                    let name = match p {
                        6 => "TCP".to_string(),
                        17 => "UDP".to_string(),
                        1 => "ICMP".to_string(),
                        58 => "ICMPv6".to_string(),
                        other => format!("proto {other}"),
                    };
                    format!("{name} {n}")
                })
                .collect();
            ui.label(names.join(" · "));
            ui.end_row();
        }
        if !g.other_protocols.is_empty() {
            ui.label("Not IP");
            let v: Vec<String> = g
                .other_protocols
                .iter()
                .map(|(t, n)| format!("0x{t:04X} ×{n}"))
                .collect();
            ui.label(v.join(", "));
            ui.end_row();
        }
        if let Some((path, n)) = &g.pcap {
            ui.label("PCAP");
            if let Some(e) = &g.pcap_error {
                ui.colored_label(Color32::from_rgb(230, 110, 110), e);
            } else if g.pcap_active {
                ui.colored_label(Color32::from_rgb(230, 90, 90), format!("● {n} packets"));
            } else {
                ui.label(format!("stopped, {n} packets"));
            }
            ui.end_row();
            ui.label("");
            ui.label(RichText::new(path.display().to_string()).small());
            ui.end_row();
        }
    });
    if !g.top.is_empty() {
        ui.add_space(4.0);
        ui.label(
            RichText::new(format!("Top flows (of {})", g.flows))
                .small()
                .strong(),
        );
        egui::Grid::new("flows")
            .num_columns(3)
            .striped(true)
            .show(ui, |ui| {
                for f in &g.top {
                    ui.label(
                        RichText::new(format!("{} → {}", f.src, f.dst))
                            .monospace()
                            .small(),
                    );
                    ui.label(RichText::new(format!("{}", f.packets)).small());
                    ui.label(RichText::new(format!("{:.1} kB", f.bytes as f64 / 1e3)).small());
                    ui.end_row();
                }
            });
    }
    if g.mpe.is_some() && g.source.is_none() {
        return; // no GSE to compare variants of
    }
    egui::CollapsingHeader::new("GSE variants")
        .id_salt("gse_variants")
        .show(ui, |ui| {
            egui::Grid::new("variants").num_columns(4).show(ui, |ui| {
                ui.label(RichText::new("variant").small());
                ui.label(RichText::new("IP").small());
                ui.label(RichText::new("reassembled").small());
                ui.label(RichText::new("CRC ok / bad").small());
                ui.end_row();
                for r in &g.variants {
                    ui.label(RichText::new(r.variant.label()).small());
                    ui.label(RichText::new(r.ip_packets.to_string()).small());
                    ui.label(RichText::new(r.stats.reassembled.to_string()).small());
                    ui.label(
                        RichText::new(format!("{} / {}", r.stats.crc_ok, r.stats.crc_bad)).small(),
                    );
                    ui.end_row();
                }
            });
        });
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

/// `live`: Identify's live view of the carrier, when it is running.
fn identification_card(
    ui: &mut Ui,
    id: &Identification,
    abs_center: f64,
    live: Option<&CarrierState>,
) {
    let (headline, col) = match &id.verdict {
        Verdict::NoSignal => ("No signal".to_string(), Color32::GRAY),
        Verdict::Carrier => (
            "Narrow carrier".to_string(),
            Color32::from_rgb(200, 200, 120),
        ),
        Verdict::DvbS2(d) => {
            let kind = if d.is_s2x() { "DVB-S2X" } else { "DVB-S2" };
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
                        .filter_map(|&m| decdvb_core::modcod(m, decdvb_core::FecFrame::Normal))
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
                if let Some(c) = live {
                    // Live: the demodulator running between identifications.
                    ui.label("Carrier");
                    if c.locked {
                        ui.colored_label(
                            Color32::from_rgb(110, 220, 110),
                            format!("locked · MER {:.1} dB (live)", c.mer_db),
                        );
                    } else {
                        ui.colored_label(Color32::from_rgb(230, 150, 90), "not locked (live)");
                    }
                    ui.end_row();
                } else if let (Some(mer), Some(coh)) = (id.mer_db, id.coherence) {
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
                    decdvb_core::modcod(m, decdvb_core::FecFrame::Normal)
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
