use crate::ipc_client::{Diagnostics, IpcClient};
use crate::{HapticParams, BUILD_HASH};
use nih_plug::prelude::nih_log;
use nih_plug::prelude::*;
use nih_plug_egui::{create_egui_editor, egui, widgets, EguiState};
use std::sync::Arc;

fn param_row<P: Param>(
    ui: &mut egui::Ui,
    label: &str,
    param: &P,
    setter: &ParamSetter<'_>,
    slider_width: f32,
) {
    egui::Grid::new(label).num_columns(2).show(ui, |ui| {
        ui.label(label);
        ui.add(widgets::ParamSlider::for_param(param, setter).with_width(slider_width));
        ui.end_row();
    });
}

pub fn create(
    params: Arc<HapticParams>,
    ipc_client: Arc<IpcClient>,
    diag: Arc<Diagnostics>,
) -> Option<Box<dyn Editor>> {
    nih_log!("Creating plugin editor UI");
    let editor_state = EguiState::from_size(520, 330);

    create_egui_editor(
        editor_state,
        params.clone(),
        |_ctx, _setter| {
            nih_log!("Editor UI initialized");
        },
        move |egui_ctx, setter, _state| {
            // Repaint steadily so the atomic diagnostic counters stay live.
            egui_ctx.request_repaint_after(std::time::Duration::from_millis(100));

            egui::CentralPanel::default().show(egui_ctx, |ui| {
                ui.spacing_mut().item_spacing.y = 4.0;
                let connected = ipc_client.is_connected();
                let d = diag.snapshot();
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let status = if connected {
                            "● connected"
                        } else {
                            "● retrying"
                        };
                        let colour = if connected {
                            egui::Color32::GREEN
                        } else {
                            egui::Color32::RED
                        };
                        ui.colored_label(colour, status)
                            .on_hover_text(if connected {
                                "Configuration and MIDI are being sent to the server"
                            } else {
                                "The plugin is retrying the server connection"
                            });
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            ui.heading("Haptic Controller");
                        });
                    });
                });

                let mut identity = format!(
                    "build {BUILD_HASH} · p{} · instance #{:04x}",
                    haptic_protocol::PROTOCOL_VERSION,
                    diag.instance_id & 0xffff,
                );
                if d.connect_generation > 1 {
                    identity.push_str(&format!(" · reconnects {}", d.connect_generation - 1));
                }
                ui.add(
                    egui::Label::new(egui::RichText::new(&identity).monospace().weak()).truncate(),
                )
                .on_hover_text(&identity);

                // The note-type configuration this instance is sending. These
                // are host-automatable VST parameters (configuration), distinct
                // from the performance gestures carried by MIDI/MPE.
                ui.group(|ui| {
                    ui.strong("configuration");
                    let tw =
                        params.stimulus_type.value() == crate::StimulusTypeParam::TravellingWave;

                    param_row(ui, "type", &params.stimulus_type, setter, 220.0);

                    if tw {
                        param_row(ui, "scale", &params.tw_scale_mode, setter, 260.0);
                        match params.tw_scale_mode.value() {
                            crate::SpatialScaleModeParam::Speed => {
                                param_row(ui, "speed", &params.wave_speed, setter, 300.0)
                            }
                            crate::SpatialScaleModeParam::Wavelength => {
                                param_row(ui, "wavelength", &params.tw_wavelength, setter, 300.0)
                            }
                        }
                    } else {
                        param_row(ui, "speed", &params.wave_speed, setter, 300.0);
                    }

                    param_row(ui, "decay knee", &params.atten_d0, setter, 300.0);
                    param_row(ui, "exponent", &params.atten_exponent, setter, 300.0);
                });

                // Incoming-MIDI diagnostics: confirms events are arriving from
                // the host and being sent. `dropped` counts sends that failed
                // (queue full / server down) — a fast pointer at why notes have
                // "no effect".
                ui.group(|ui| {
                    ui.strong("MIDI");
                    ui.horizontal(|ui| {
                        let spacing = ui.spacing().item_spacing.x;
                        let cell_width = ((ui.available_width() - 3.0 * spacing) / 4.0).max(48.0);
                        let cell_size = [cell_width, ui.spacing().interact_size.y];
                        ui.add_sized(cell_size, egui::Label::new(format!("on {}", d.notes_on)));
                        ui.add_sized(cell_size, egui::Label::new(format!("off {}", d.notes_off)));
                        ui.add_sized(
                            cell_size,
                            egui::Label::new(format!("MPE {}", d.mpe_updates)),
                        );
                        let dropped = d.sends_dropped;
                        let col = if dropped > 0 {
                            egui::Color32::from_rgb(220, 140, 60)
                        } else {
                            egui::Color32::GRAY
                        };
                        ui.add_sized(
                            cell_size,
                            egui::Label::new(
                                egui::RichText::new(format!("dropped {dropped}")).color(col),
                            )
                            .truncate(),
                        );
                    });
                    if d.notes_on == 0 && d.notes_off == 0 && d.mpe_updates == 0 {
                        ui.colored_label(
                            egui::Color32::from_rgb(220, 140, 60),
                            "No MIDI yet — check the track's routing",
                        );
                    }
                });
            });
        },
    )
}
