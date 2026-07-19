//! Loadable TOML configuration: marker colors, overlay sizes, the beacon
//! distance readout, track recording, and the BLE beacon settings.
//!
//! Schema (all fields optional; missing ones keep their defaults):
//!
//! ```toml
//! [colors]
//! track = "#0078ff"   # phone track, heading arrow, position dot
//! fixed = "#ff5028"   # BLE beacon marker, distance line, beacon path
//!
//! [sizes]             # screen points; each overlay is sized independently
//! marker = 8.0        # current-position dot radius
//! beacon = 6.0        # beacon dot radius
//! track = 3.0         # track polyline width (phone track and beacon path)
//! distance_line = 3.0 # user<->beacon line width
//! distance_text = 14.0 # beacon-distance label font size
//!
//! [distance]
//! show = false        # draw the distance label on the user<->beacon line
//! units = "metric"    # "metric" (km/m) or "imperial" (mi/ft)
//! dotted = true       # draw the user<->beacon line dotted rather than solid
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

/// Sizes, in screen points, for the drawn map overlays. Each is independent so
/// the markers, lines, and label can be tuned separately.
#[derive(Clone, Copy)]
pub struct MarkerSizes {
    /// Radius of the current-position dot.
    pub marker: f32,
    /// Radius of the BLE beacon dot.
    pub beacon: f32,
    /// Width of the recorded track polylines (phone track and beacon path).
    pub track: f32,
    /// Width of the line drawn from the current position to the beacon.
    pub distance_line: f32,
    /// Font size of the distance label drawn on that line.
    pub distance_text: f32,
}

impl Default for MarkerSizes {
    fn default() -> Self {
        Self {
            marker: 8.0,
            beacon: 6.0,
            track: 3.0,
            distance_line: 3.0,
            distance_text: 14.0,
        }
    }
}

/// Unit system for the beacon-distance label.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DistanceUnits {
    /// Kilometers and meters.
    Metric,
    /// Miles and feet.
    Imperial,
}

impl DistanceUnits {
    /// Format a distance given in meters: the larger unit once past one of it,
    /// otherwise the smaller whole unit.
    pub fn format(self, meters: f64) -> String {
        match self {
            DistanceUnits::Metric => {
                if meters >= 1000.0 {
                    format!("{:.2} km", meters / 1000.0)
                } else {
                    format!("{meters:.0} m")
                }
            }
            DistanceUnits::Imperial => {
                const M_PER_MILE: f64 = 1609.344;
                const FT_PER_M: f64 = 3.280_84;
                if meters >= M_PER_MILE {
                    format!("{:.2} mi", meters / M_PER_MILE)
                } else {
                    format!("{:.0} ft", meters * FT_PER_M)
                }
            }
        }
    }

    /// Parse the TOML `distance.units` string (case-insensitive).
    fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_lowercase().as_str() {
            "metric" | "km" | "km/m" | "m" => Ok(DistanceUnits::Metric),
            "imperial" | "mi" | "mi/ft" | "ft" => Ok(DistanceUnits::Imperial),
            other => Err(format!(
                "invalid distance.units {other:?}, expected \"metric\" or \"imperial\""
            )),
        }
    }
}

/// Beacon-distance readout settings.
#[derive(Clone, Copy)]
pub struct DistanceSettings {
    /// Draw the distance label on the line to the beacon.
    pub show: bool,
    /// Which unit system the label uses.
    pub units: DistanceUnits,
    /// Draw the line to the beacon dotted rather than solid.
    pub dotted: bool,
}

impl Default for DistanceSettings {
    fn default() -> Self {
        Self {
            show: false,
            units: DistanceUnits::Metric,
            dotted: true,
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
    pub sizes: MarkerSizes,
    pub distance: DistanceSettings,
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
    sizes: RawSizes,
    #[serde(default)]
    distance: RawDistance,
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
struct RawSizes {
    marker: Option<f32>,
    beacon: Option<f32>,
    track: Option<f32>,
    distance_line: Option<f32>,
    distance_text: Option<f32>,
}

#[derive(Deserialize, Default)]
struct RawDistance {
    show: Option<bool>,
    units: Option<String>,
    dotted: Option<bool>,
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

/// Validate an overlay size: finite and strictly positive.
fn parse_size(name: &str, v: f32) -> Result<f32, String> {
    if !v.is_finite() || v <= 0.0 {
        return Err(format!("sizes.{name} must be > 0, got {v}"));
    }
    Ok(v)
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
        if let Some(v) = raw.sizes.marker {
            config.sizes.marker = parse_size("marker", v)?;
        }
        if let Some(v) = raw.sizes.beacon {
            config.sizes.beacon = parse_size("beacon", v)?;
        }
        if let Some(v) = raw.sizes.track {
            config.sizes.track = parse_size("track", v)?;
        }
        if let Some(v) = raw.sizes.distance_line {
            config.sizes.distance_line = parse_size("distance_line", v)?;
        }
        if let Some(v) = raw.sizes.distance_text {
            config.sizes.distance_text = parse_size("distance_text", v)?;
        }
        if let Some(v) = raw.distance.show {
            config.distance.show = v;
        }
        if let Some(s) = raw.distance.units {
            config.distance.units = DistanceUnits::parse(&s)?;
        }
        if let Some(v) = raw.distance.dotted {
            config.distance.dotted = v;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metric_switches_km_at_a_kilometer() {
        assert_eq!(DistanceUnits::Metric.format(0.0), "0 m");
        assert_eq!(DistanceUnits::Metric.format(940.0), "940 m");
        assert_eq!(DistanceUnits::Metric.format(1000.0), "1.00 km");
        assert_eq!(DistanceUnits::Metric.format(2500.0), "2.50 km");
    }

    #[test]
    fn imperial_switches_miles_at_a_mile() {
        assert_eq!(DistanceUnits::Imperial.format(0.0), "0 ft");
        // Just under a mile stays in feet.
        assert_eq!(DistanceUnits::Imperial.format(1609.0), "5279 ft");
        assert_eq!(DistanceUnits::Imperial.format(1609.344), "1.00 mi");
        assert_eq!(DistanceUnits::Imperial.format(3218.688), "2.00 mi");
    }

    #[test]
    fn units_parse_is_case_insensitive_and_aliased() {
        assert_eq!(DistanceUnits::parse("Metric").unwrap(), DistanceUnits::Metric);
        assert_eq!(DistanceUnits::parse("km/m").unwrap(), DistanceUnits::Metric);
        assert_eq!(DistanceUnits::parse("IMPERIAL").unwrap(), DistanceUnits::Imperial);
        assert_eq!(DistanceUnits::parse("mi/ft").unwrap(), DistanceUnits::Imperial);
        assert!(DistanceUnits::parse("furlongs").is_err());
    }

    #[test]
    fn sizes_reject_non_positive() {
        assert!(super::AppConfig::from_toml("[sizes]\nmarker = 0.0").is_err());
        assert!(super::AppConfig::from_toml("[sizes]\nbeacon = -1.0").is_err());
        let cfg = super::AppConfig::from_toml("[sizes]\nmarker = 12.0").unwrap();
        assert_eq!(cfg.sizes.marker, 12.0);
    }
}
