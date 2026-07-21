use nih_plug_egui::{create_egui_editor, egui, widgets, EguiState};
use nih_plug::prelude::*;
use nih_plug::prelude::nih_log;
use std::sync::Arc;
use parking_lot::Mutex;
use crate::ipc_client::{Diagnostics, IpcClient};
use crate::HapticParams;

pub fn create(
    params: Arc<HapticParams>,
    ipc_client: Arc<IpcClient>,
    diag: Arc<Mutex<Diagnostics>>,
) -> Option<Box<dyn Editor>> {
    nih_log!("Creating plugin editor UI");
    let editor_state = EguiState::from_size(560, 620);

    create_egui_editor(
        editor_state,
        params.clone(),
        |_ctx, _setter| {
            nih_log!("Editor UI initialized");
        },
        move |egui_ctx, setter, _state| {
            // Repaint steadily so the diagnostics (counts, last events) stay live.
            egui_ctx.request_repaint_after(std::time::Duration::from_millis(100));

            egui::CentralPanel::default().show(egui_ctx, |ui| {
                ui.heading("Haptic Controller");
                ui.label(
                    "Controller client — sends this instance's note-type configuration \
                     and MIDI to the server. Field visualisation lives in haptic-viewer.",
                );

                ui.separator();

                let connected = ipc_client.is_connected();
                let d = diag.lock();
                ui.horizontal(|ui| {
                    ui.label("Server:");
                    if connected {
                        ui.colored_label(egui::Color32::GREEN, "● connected");
                    } else {
                        ui.colored_label(egui::Color32::RED, "● disconnected (retrying)");
                    }
                    ui.separator();
                    ui.label(format!("instance {:04x}", d.instance_id & 0xffff));
                    if d.connect_generation > 1 {
                        ui.separator();
                        ui.label(format!("reconnects: {}", d.connect_generation - 1));
                    }
                });

                ui.separator();

                // The note-type configuration this instance is sending. These
                // are host-automatable VST parameters (configuration), distinct
                // from the performance gestures carried by MIDI/MPE.
                ui.group(|ui| {
                    ui.label("Note type — configuration sent to the server");
                    egui::Grid::new("note_type_config").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                        ui.label("Stimulus type:");
                        ui.add(widgets::ParamSlider::for_param(&params.stimulus_type, setter));
                        ui.end_row();
                        ui.label("Wave speed:");
                        ui.add(widgets::ParamSlider::for_param(&params.wave_speed, setter));
                        ui.end_row();
                    });
                });

                ui.separator();

                // Incoming-MIDI diagnostics: confirms events are arriving from
                // the host and being sent. `dropped` counts sends that failed
                // (queue full / server down) — a fast pointer at why notes have
                // "no effect".
                ui.group(|ui| {
                    ui.label("Incoming MIDI");
                    ui.horizontal(|ui| {
                        ui.label(format!("note-on: {}", d.notes_on));
                        ui.separator();
                        ui.label(format!("note-off: {}", d.notes_off));
                        ui.separator();
                        ui.label(format!("mpe: {}", d.mpe_updates));
                        ui.separator();
                        let dropped = d.sends_dropped;
                        let col = if dropped > 0 { egui::Color32::from_rgb(220, 140, 60) } else { egui::Color32::GRAY };
                        ui.colored_label(col, format!("dropped: {dropped}"));
                    });
                    if d.notes_on == 0 && d.notes_off == 0 && d.mpe_updates == 0 {
                        ui.colored_label(
                            egui::Color32::from_rgb(220, 140, 60),
                            "No MIDI received yet — check the track's MIDI routing to this plugin.",
                        );
                    }
                });

                ui.separator();

                ui.group(|ui| {
                    ui.label("Recent events");
                    egui::ScrollArea::vertical().max_height(200.0).stick_to_bottom(true).show(ui, |ui| {
                        if d.events.is_empty() {
                            ui.weak("(none)");
                        }
                        for line in d.events.iter() {
                            ui.monospace(line);
                        }
                    });
                });

                ui.separator();
                ui.weak(format!("Plugin v{} · socket /tmp/haptic-vst.sock", crate::HapticPlugin::VERSION));
            });
        },
    )
}