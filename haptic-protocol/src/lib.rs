use serde::{Serialize, Deserialize};

#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
pub struct MpeData {
    pub pressure: f32,      // 0.0-1.0
    pub pitch_bend: f32,    // -1.0 to 1.0
    pub timbre: f32,        // 0.0-1.0
}

impl Default for MpeData {
    fn default() -> Self {
        Self {
            pressure: 0.0,
            pitch_bend: 0.0,
            timbre: 0.5,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum HapticCommand {
    NoteOn {
        timestamp_us: u64,
        note: u8,
        velocity: u8,
        channel: u8,
        mpe: MpeData,
    },
    NoteOff {
        timestamp_us: u64,
        note: u8,
        channel: u8,
    },
    MpeUpdate {
        timestamp_us: u64,
        channel: u8,
        mpe: MpeData,
    },
    Panic,              // Stop all
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum ServerStatus {
    TransducerLevels {
        timestamp_us: u64,
        levels: [f32; 32],
    },
    PerformanceMetrics {
        active_stimuli: u8,
        cpu_percent: u8,
    },
}

pub const SOCKET_PATH: &str = "/tmp/haptic-vst.sock";