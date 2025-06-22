use haptic_plugin::HapticPlugin;
use nih_plug::wrapper::standalone::nih_export_standalone;

fn main() {
    nih_export_standalone::<HapticPlugin>();
}