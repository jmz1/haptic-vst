// use haptic_plugin::HapticPlugin;
use nih_plug::wrapper::standalone::nih_export_standalone;
// use nih_plug::prelude::*;

fn main() {
    nih_export_standalone::<haptic_plugin::HapticPlugin>();
}