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
                
                ui.separator();
                
                // Connection status and latest server levels
                let client_guard = ipc_client.lock();
                let connected = client_guard.as_ref().map_or(false, |c| c.is_connected());
                let levels = client_guard.as_ref().map(|c| c.levels()).unwrap_or_default();
                drop(client_guard);

                // Live levels arrive at ~60 Hz; keep the UI repainting
                if connected {
                    egui_ctx.request_repaint();
                }
                
                ui.horizontal(|ui| {
                    ui.label("Server Status:");
                    if connected {
                        ui.colored_label(egui::Color32::GREEN, "Connected");
                    } else {
                        ui.colored_label(egui::Color32::RED, "Disconnected");
                    }
                });
                
                ui.separator();
                
                // Plugin parameters (pushed to the server on change)
                ui.group(|ui| {
                    ui.label("Stimulus Parameters");
                    ui.horizontal(|ui| {
                        ui.label("Wave speed:");
                        ui.add(widgets::ParamSlider::for_param(&params.wave_speed, setter));
                    });
                    ui.horizontal(|ui| {
                        ui.label("Stimulus type:");
                        ui.add(widgets::ParamSlider::for_param(&params.stimulus_type, setter));
                    });
                });
                
                ui.separator();
                
                // Transducer visualization (live RMS levels from the server)
                ui.group(|ui| {
                    ui.label("Transducer Array (32 channels, live RMS)");
                    
                    let size = egui::Vec2::new(240.0, 480.0);
                    let (response, painter) = ui.allocate_painter(size, egui::Sense::hover());
                    
                    // Draw 32 transducer indicators matching the server's
                    // default layout: 4 across the 1 m width, 8 along the
                    // 2 m length, channels running across the width first
                    let rect = response.rect;
                    let grid_cols = 4;
                    let grid_rows = 8;
                    
                    for i in 0..32 {
                        let row = i / grid_cols;
                        let col = i % grid_cols;
                        let x = rect.left() + (col as f32 + 0.5) * rect.width() / grid_cols as f32;
                        let y = rect.top() + (row as f32 + 0.5) * rect.height() / grid_rows as f32;
                        
                        // Draw transducer as circle, brightness following its level.
                        // RMS of a full-scale sine is ~0.707; headroom factor 3
                        // makes typical levels visible.
                        let radius = 8.0;
                        let color = if connected {
                            let intensity = (levels[i] * 3.0).clamp(0.0, 1.0);
                            let base = 70.0;
                            egui::Color32::from_rgb(
                                base as u8,
                                (base + (255.0 - base) * intensity) as u8,
                                (base + (160.0 - base) * intensity) as u8,
                            )
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
                    ui.label("Default layout: 4 × 8 over 1 m × 2 m (see haptic.toml)");
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
                    ui.label("4. Stimulus type and wave speed are set by the parameters above");
                    ui.label("5. Velocity controls intensity; MPE pressure/bend/slide modulate the voice");
                });
            });
        },
    )
}