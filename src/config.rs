//! Loadable TOML configuration: marker colors, track recording, and the BLE
//! beacon settings.
//!
//! Schema (all fields optional; missing ones keep their defaults):
//!
//! ```toml
//! [colors]
//! track = "#0078ff"   # phone track, heading arrow, position dot
//! fixed = "#ff5028"   # BLE beacon marker, distance line, beacon path
//!
//! [ble]
//! enabled = true      # master switch for the BLE GPS source
//! show_path = false   # draw the path of the incoming BLE GPS data
//! mac = "AA:BB:CC:DD:EE:FF"  # pin a specific device; omit to scan by service
//!
//! [track]
//! min_distance = 3.0  # meters of movement before a new track point is recorded
//! ```

use egui::Color32;
use serde::Deserialize;

/// Colors used to draw the map markers.
#[derive(Clone, Copy)]
pub struct MarkerColors {
    /// Track polyline, heading arrow, and the current-position dot.
    pub track: Color32,
    /// BLE beacon marker, the line drawn to it, and its path.
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

/// BLE beacon settings.
#[derive(Clone)]
pub struct BleSettings {
    /// Connect to the beacon at all.
    pub enabled: bool,
    /// Draw the path of the incoming BLE GPS data on the map.
    pub show_path: bool,
    /// Pin a specific device MAC; `None` scans for the GPS service.
    pub mac: Option<String>,
}

impl Default for BleSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            show_path: false,
            mac: None,
        }
    }
}

/// Track recording settings.
#[derive(Clone, Copy)]
pub struct TrackSettings {
    /// Minimum distance in meters from the last recorded point before another
    /// is appended to a track. Decimates GPS jitter; 0 records every fix.
    pub min_distance: f64,
}

impl Default for TrackSettings {
    fn default() -> Self {
        Self { min_distance: 3.0 }
    }
}

/// Everything a config file can carry.
#[derive(Clone, Default)]
pub struct AppConfig {
    pub colors: MarkerColors,
    pub ble: BleSettings,
    pub track: TrackSettings,
}

/// Mirrors the TOML shape; every field optional so a partial file keeps the
/// defaults for whatever it leaves out.
#[derive(Deserialize, Default)]
struct RawConfig {
    #[serde(default)]
    colors: RawColors,
    #[serde(default)]
    ble: RawBle,
    #[serde(default)]
    track: RawTrack,
}

#[derive(Deserialize, Default)]
struct RawColors {
    track: Option<String>,
    fixed: Option<String>,
}

#[derive(Deserialize, Default)]
struct RawBle {
    enabled: Option<bool>,
    show_path: Option<bool>,
    mac: Option<String>,
}

#[derive(Deserialize, Default)]
struct RawTrack {
    min_distance: Option<f64>,
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

impl AppConfig {
    /// Read the config from a TOML file at `path`. Missing fields fall back
    /// to the defaults; a returned `Err` is a human-readable message for the
    /// UI.
    pub fn load(path: &str) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
        Self::from_toml(&text)
    }

    fn from_toml(text: &str) -> Result<Self, String> {
        let raw: RawConfig = toml::from_str(text).map_err(|e| e.to_string())?;
        let mut config = Self::default();
        if let Some(s) = raw.colors.track {
            config.colors.track = parse_hex(&s)?;
        }
        if let Some(s) = raw.colors.fixed {
            config.colors.fixed = parse_hex(&s)?;
        }
        if let Some(v) = raw.ble.enabled {
            config.ble.enabled = v;
        }
        if let Some(v) = raw.ble.show_path {
            config.ble.show_path = v;
        }
        // Treat an empty string as unset so a template line can stay in the
        // file.
        config.ble.mac = raw.ble.mac.filter(|m| !m.trim().is_empty());
        if let Some(v) = raw.track.min_distance {
            if !v.is_finite() || v < 0.0 {
                return Err(format!("track.min_distance must be >= 0, got {v}"));
            }
            config.track.min_distance = v;
        }
        Ok(config)
    }
}
