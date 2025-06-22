use nih_plug_egui::{create_egui_editor, egui, EguiState};
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
        move |egui_ctx, _setter, _state| {
            egui::CentralPanel::default().show(egui_ctx, |ui| {
                ui.heading("Haptic Controller");
                
                ui.separator();
                
                // Connection status
                let client_guard = ipc_client.lock();
                let connected = client_guard.as_ref().map_or(false, |c| c.is_connected());
                drop(client_guard);
                
                ui.horizontal(|ui| {
                    ui.label("Server Status:");
                    if connected {
                        ui.colored_label(egui::Color32::GREEN, "Connected");
                    } else {
                        ui.colored_label(egui::Color32::RED, "Disconnected");
                    }
                });
                
                ui.separator();
                
                // Information display (no plugin parameters currently)
                ui.group(|ui| {
                    ui.label("Wave Parameters");
                    ui.label("Wave speed is automatically calculated on the server based on note velocity");
                    ui.label("Low velocity notes: Slower wave propagation");
                    ui.label("High velocity notes: Faster wave propagation");
                });
                
                ui.separator();
                
                // Transducer visualization
                ui.group(|ui| {
                    ui.label("Transducer Array (32 channels)");
                    
                    let size = egui::Vec2::new(400.0, 200.0);
                    let (response, painter) = ui.allocate_painter(size, egui::Sense::hover());
                    
                    // Draw 32 transducer indicators in 4x8 grid
                    let rect = response.rect;
                    let grid_cols = 8;
                    let grid_rows = 4;
                    
                    for i in 0..32 {
                        let row = i / grid_cols;
                        let col = i % grid_cols;
                        let x = rect.left() + (col as f32 + 0.5) * rect.width() / grid_cols as f32;
                        let y = rect.top() + (row as f32 + 0.5) * rect.height() / grid_rows as f32;
                        
                        // Draw transducer as circle
                        let radius = 8.0;
                        let color = if connected {
                            egui::Color32::from_gray(100)
                        } else {
                            egui::Color32::from_gray(64)
                        };
                        
                        painter.circle_filled(
                            egui::pos2(x, y),
                            radius,
                            color,
                        );
                        
                        // Draw transducer number
                        painter.text(
                            egui::pos2(x, y),
                            egui::Align2::CENTER_CENTER,
                            format!("{}", i + 1),
                            egui::FontId::monospace(10.0),
                            egui::Color32::WHITE,
                        );
                    }
                    
                    // Draw grid lines
                    let stroke = egui::Stroke::new(1.0, egui::Color32::from_gray(50));
                    for col in 0..=grid_cols {
                        let x = rect.left() + col as f32 * rect.width() / grid_cols as f32;
                        painter.line_segment(
                            [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
                            stroke,
                        );
                    }
                    for row in 0..=grid_rows {
                        let y = rect.top() + row as f32 * rect.height() / grid_rows as f32;
                        painter.line_segment(
                            [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
                            stroke,
                        );
                    }
                    
                    // Add grid labels
                    ui.label("Grid spacing: 5cm × 5cm");
                });
                
                ui.separator();
                
                // System information
                ui.group(|ui| {
                    ui.label("System Information");
                    ui.label(format!("Plugin Version: {}", crate::HapticPlugin::VERSION));
                    ui.label("Protocol: Unix Domain Socket");
                    ui.label("Socket Path: /tmp/haptic-vst.sock");
                });
                
                ui.separator();
                
                // Instructions
                ui.group(|ui| {
                    ui.label("Instructions");
                    ui.label("1. Start the haptic-server executable");
                    ui.label("2. Load this plugin in your DAW");
                    ui.label("3. Send MIDI notes to trigger haptic stimuli");
                    ui.label("4. Low velocity notes → Wave stimuli");
                    ui.label("5. High velocity notes → Standing wave stimuli");
                });
            });
        },
    )
}