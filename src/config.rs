//! Loadable TOML configuration. For now it only carries the marker colors, but
//! the schema is meant to grow (fixed point, initial view, ...) later.

use egui::Color32;
use serde::Deserialize;

/// Colors used to draw the map markers.
#[derive(Clone, Copy)]
pub struct MarkerColors {
    /// Track polyline, heading arrow, and the current-position dot.
    pub track: Color32,
    /// Fixed reference point and the line drawn to it.
    pub fixed: Color32,
}

impl Default for MarkerColors {
    fn default() -> Self {
        Self {
            track: Color32::from_rgb(0, 120, 255),
            fixed: Color32::from_rgb(255, 80, 40),
        }
    }
}

/// Mirrors the TOML shape; every field optional so a partial file keeps the
/// defaults for whatever it leaves out.
#[derive(Deserialize, Default)]
struct RawConfig {
    #[serde(default)]
    colors: RawColors,
}

#[derive(Deserialize, Default)]
struct RawColors {
    track: Option<String>,
    fixed: Option<String>,
}

/// Parse a `#rrggbb` (or bare `rrggbb`) hex string into a color.
fn parse_hex(s: &str) -> Result<Color32, String> {
    let h = s.trim().trim_start_matches('#');
    if h.len() != 6 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("invalid hex color {s:?}, expected #rrggbb"));
    }
    let n = u32::from_str_radix(h, 16).map_err(|_| format!("invalid hex color {s:?}"))?;
    Ok(Color32::from_rgb((n >> 16) as u8, (n >> 8) as u8, n as u8))
}

impl MarkerColors {
    /// Read marker colors from a TOML file at `path`. Missing fields fall back to
    /// the defaults; a returned `Err` is a human-readable message for the UI.
    pub fn load(path: &str) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
        Self::from_toml(&text)
    }

    fn from_toml(text: &str) -> Result<Self, String> {
        let raw: RawConfig = toml::from_str(text).map_err(|e| e.to_string())?;
        let mut colors = Self::default();
        if let Some(s) = raw.colors.track {
            colors.track = parse_hex(&s)?;
        }
        if let Some(s) = raw.colors.fixed {
            colors.fixed = parse_hex(&s)?;
        }
        Ok(colors)
    }
}
