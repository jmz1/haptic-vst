//! Transducer layout configuration.
//!
//! Loaded from TOML at startup and hot-reloaded when the file changes.
//! All distances are physical metres — the wave-propagation model derives
//! per-transducer delays from real distances and wave speed (m/s), so the
//! layout must use real dimensions, not normalised coordinates.

use serde::Deserialize;
use crate::engine::TRANSDUCER_COUNT;

/// Default table extents: 1 m across (x), 2 m along (y).
pub const DEFAULT_TABLE_WIDTH_M: f32 = 1.0;
pub const DEFAULT_TABLE_LENGTH_M: f32 = 2.0;

/// Resolved layout consumed by the engine.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TransducerLayout {
    /// (x, y) in metres, origin at one corner of the table.
    pub positions: [(f32, f32); TRANSDUCER_COUNT],
    /// Linear output gain per transducer (1.0 = unity).
    pub gains: [f32; TRANSDUCER_COUNT],
    /// (width, length) of the table in metres, for visualisation.
    pub table_m: (f32, f32),
}

impl Default for TransducerLayout {
    /// 4 columns across the 1 m width, 8 rows along the 2 m length,
    /// cell-centred. Channels run across the width first: channel = row*4+col.
    fn default() -> Self {
        Self::grid(4, 8, DEFAULT_TABLE_WIDTH_M, DEFAULT_TABLE_LENGTH_M, 1.0)
            .expect("default grid is valid")
    }
}

impl TransducerLayout {
    /// Cell-centred cols × rows grid over a width × length table.
    pub fn grid(cols: usize, rows: usize, width_m: f32, length_m: f32, gain: f32) -> Result<Self, String> {
        if cols * rows != TRANSDUCER_COUNT {
            return Err(format!(
                "grid is {}x{} = {} transducers; exactly {} required",
                cols, rows, cols * rows, TRANSDUCER_COUNT
            ));
        }
        if !(width_m > 0.0 && length_m > 0.0) {
            return Err("table dimensions must be positive".into());
        }
        let mut positions = [(0.0, 0.0); TRANSDUCER_COUNT];
        for (i, pos) in positions.iter_mut().enumerate() {
            let col = i % cols;
            let row = i / cols;
            *pos = (
                (col as f32 + 0.5) * width_m / cols as f32,
                (row as f32 + 0.5) * length_m / rows as f32,
            );
        }
        Ok(Self {
            positions,
            gains: [gain; TRANSDUCER_COUNT],
            table_m: (width_m, length_m),
        })
    }
}

// ---------------------------------------------------------------------------
// TOML schema
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    table: Option<RawTable>,
    grid: Option<RawGrid>,
    #[serde(default, rename = "transducer")]
    transducers: Vec<RawTransducer>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTable {
    width_m: f32,
    length_m: f32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGrid {
    cols: usize,
    rows: usize,
    gain: Option<f32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTransducer {
    channel: usize,
    x: f32,
    y: f32,
    gain: Option<f32>,
}

/// Parse a TOML document into a layout. The `[grid]` section (or the default
/// 4×8 grid) lays out all transducers; `[[transducer]]` entries then override
/// individual channels.
pub fn parse_layout(text: &str) -> Result<TransducerLayout, String> {
    let raw: RawConfig = toml::from_str(text).map_err(|e| format!("TOML parse error: {}", e))?;

    let (width_m, length_m) = match &raw.table {
        Some(t) => (t.width_m, t.length_m),
        None => (DEFAULT_TABLE_WIDTH_M, DEFAULT_TABLE_LENGTH_M),
    };

    let mut layout = match &raw.grid {
        Some(g) => TransducerLayout::grid(g.cols, g.rows, width_m, length_m, g.gain.unwrap_or(1.0))?,
        None => TransducerLayout::grid(4, 8, width_m, length_m, 1.0)?,
    };

    for t in &raw.transducers {
        if t.channel >= TRANSDUCER_COUNT {
            return Err(format!(
                "transducer channel {} out of range (0-{})",
                t.channel,
                TRANSDUCER_COUNT - 1
            ));
        }
        if !(t.x.is_finite() && t.y.is_finite()) {
            return Err(format!("transducer {} has non-finite position", t.channel));
        }
        layout.positions[t.channel] = (t.x, t.y);
        if let Some(gain) = t.gain {
            layout.gains[t.channel] = gain;
        }
    }

    for (i, &gain) in layout.gains.iter().enumerate() {
        if !gain.is_finite() || gain < 0.0 {
            return Err(format!("gain for transducer {} must be finite and >= 0", i));
        }
    }

    Ok(layout)
}

pub fn load_layout(path: &std::path::Path) -> Result<TransducerLayout, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {}", path.display(), e))?;
    parse_layout(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_layout_is_cell_centred_4x8_over_1x2m() {
        let layout = TransducerLayout::default();
        // First cell centre and last cell centre
        assert_eq!(layout.positions[0], (0.125, 0.125));
        assert_eq!(layout.positions[3], (0.875, 0.125)); // end of first row
        assert_eq!(layout.positions[31], (0.875, 1.875));
        assert!(layout.gains.iter().all(|&g| g == 1.0));
        // All positions inside the table
        for &(x, y) in layout.positions.iter() {
            assert!(x > 0.0 && x < DEFAULT_TABLE_WIDTH_M);
            assert!(y > 0.0 && y < DEFAULT_TABLE_LENGTH_M);
        }
    }

    #[test]
    fn empty_config_gives_default_layout() {
        assert_eq!(parse_layout("").unwrap(), TransducerLayout::default());
    }

    #[test]
    fn grid_with_custom_table_and_gain() {
        let layout = parse_layout(
            r#"
            [table]
            width_m = 2.0
            length_m = 4.0

            [grid]
            cols = 8
            rows = 4
            gain = 0.5
            "#,
        )
        .unwrap();
        assert_eq!(layout.positions[0], (0.125, 0.5));
        assert_eq!(layout.positions[7], (1.875, 0.5));
        assert_eq!(layout.positions[31], (1.875, 3.5));
        assert!(layout.gains.iter().all(|&g| g == 0.5));
    }

    #[test]
    fn transducer_entries_override_grid() {
        let layout = parse_layout(
            r#"
            [[transducer]]
            channel = 5
            x = 0.42
            y = 1.0
            gain = 0.25
            "#,
        )
        .unwrap();
        assert_eq!(layout.positions[5], (0.42, 1.0));
        assert_eq!(layout.gains[5], 0.25);
        // Other channels untouched
        assert_eq!(layout.positions[0], TransducerLayout::default().positions[0]);
        assert_eq!(layout.gains[0], 1.0);
    }

    #[test]
    fn invalid_configs_are_rejected() {
        // Wrong transducer count
        assert!(parse_layout("[grid]\ncols = 4\nrows = 4").is_err());
        // Channel out of range
        assert!(parse_layout("[[transducer]]\nchannel = 32\nx = 0.0\ny = 0.0").is_err());
        // Negative gain
        assert!(parse_layout("[[transducer]]\nchannel = 0\nx = 0.0\ny = 0.0\ngain = -1.0").is_err());
        // Unknown field (typo protection)
        assert!(parse_layout("[table]\nwidth = 1.0\nlength_m = 2.0").is_err());
    }
}
