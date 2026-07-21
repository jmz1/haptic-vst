use nih_plug_egui::{create_egui_editor, egui, widgets, EguiState};
use nih_plug::prelude::*;
use nih_plug::prelude::nih_log;
use std::sync::Arc;
use parking_lot::Mutex;
use crate::{HapticParams, IpcClient};

pub fn create(
    params: Arc<HapticParams>,
    ipc_client: Arc<Mutex<Option<IpcClient>>>,
) -> Option<Box<dyn Editor>> {
    nih_log!("Creating plugin editor UI");
    let editor_state = EguiState::from_size(800, 600);
    nih_log!("Editor UI size: 800x600");
    
    create_egui_editor(
        editor_state,
        params.clone(),
        |_ctx, _setter| {
            nih_log!("Editor UI initialized");
        },
        move |egui_ctx, setter, _state| {
            egui::CentralPanel::default().show(egui_ctx, |ui| {
                ui.heading("Haptic Controller");
                ui.label(
                    "Controller client: sends this instance's note-type configuration \
                     to the server. Whole-system visualisation lives in haptic-viewer.",
                );

                ui.separator();

                // Connection status. The plugin is a pure controller now — it
                // consumes no status stream, so there is nothing to poll and no
                // reason to repaint continuously; is_connected reflects the
                // writer thread's health.
                let connected = ipc_client.lock().as_ref().map_or(false, |c| c.is_connected());
                ui.horizontal(|ui| {
                    ui.label("Server:");
                    if connected {
                        ui.colored_label(egui::Color32::GREEN, "Connected");
                    } else {
                        ui.colored_label(egui::Color32::RED, "Disconnected");
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

                ui.group(|ui| {
                    ui.label("Performance is played over MIDI/MPE:");
                    ui.label("• Note & velocity → which stimulus and how hard");
                    ui.label("• MPE pressure / bend / slide → live source modulation");
                    ui.label(format!("Plugin v{} · socket /tmp/haptic-vst.sock", crate::HapticPlugin::VERSION));
                });
            });
        },
    )
}