//! Loadable TOML configuration: marker colors, overlay sizes, the beacon
//! distance readout, track recording, and the BLE beacon settings.
//!
//! The Settings page edits these live and writes them back with [`AppConfig::save`],
//! which edits an existing file in place (comments, key order, and keys this app
//! does not know about all survive) and generates a documented one from
//! [`AppConfig::to_toml`] when there is nothing there yet.
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
//! [ble.names]         # nicknames for known boards, keyed by MAC
//! "AA:BB:CC:DD:EE:FF" = "Truck"
//!
//! [track]
//! min_distance = 3.0  # meters of movement before a new track point is recorded
//! ```

use std::collections::BTreeMap;

use egui::Color32;
use serde::Deserialize;
use toml_edit::{DocumentMut, Item, Table, Value};

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
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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

    /// The TOML spelling, the one [`Self::parse`] reads back.
    pub fn as_str(self) -> &'static str {
        match self {
            DistanceUnits::Metric => "metric",
            DistanceUnits::Imperial => "imperial",
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
    /// Nicknames for known boards, keyed by MAC. Every board runs the same
    /// firmware and so advertises the same name, which makes a scan a list of
    /// identical entries; these names are what tells them apart in the picker.
    /// Keys are normalized by [`normalize_mac`] so lookups ignore case and
    /// separator style.
    pub names: BTreeMap<String, String>,
}

impl Default for BleSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            show_path: false,
            mac: None,
            names: BTreeMap::new(),
        }
    }
}

impl BleSettings {
    /// The nickname for `mac`, if one is set.
    pub fn name_of(&self, mac: &str) -> Option<&str> {
        self.names.get(&normalize_mac(mac)).map(String::as_str)
    }

    /// Name `mac`, or forget it when `name` is blank - clearing the box is how
    /// a board leaves the picker.
    pub fn set_name(&mut self, mac: &str, name: &str) {
        let key = normalize_mac(mac);
        let name = name.trim();
        if name.is_empty() {
            self.names.remove(&key);
        } else {
            self.names.insert(key, name.to_string());
        }
    }

    /// How this device should be labelled: its nickname, or the MAC itself
    /// when it has none.
    pub fn label_of(&self, mac: &str) -> String {
        self.name_of(mac).unwrap_or(mac).to_string()
    }

    /// True when `mac` is the pinned device, comparing normalized.
    pub fn is_selected(&self, mac: &str) -> bool {
        self.mac.as_deref().map(normalize_mac) == Some(normalize_mac(mac))
    }
}

/// A MAC as a table key: uppercase, colon-separated. Boards report addresses in
/// whatever case their stack prefers and a hand-typed one may use dashes, so
/// keying on the raw string would file the same board twice.
pub fn normalize_mac(mac: &str) -> String {
    mac.trim()
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .collect::<String>()
        .to_ascii_uppercase()
        .as_bytes()
        .chunks(2)
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect::<Vec<_>>()
        .join(":")
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
    #[serde(default)]
    names: BTreeMap<String, String>,
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

/// `#rrggbb` for a color: the form [`parse_hex`] reads back.
fn hex(c: Color32) -> String {
    format!("#{:02x}{:02x}{:02x}", c.r(), c.g(), c.b())
}

/// A TOML float for an `f32` setting. Widening straight to `f64` exposes the
/// binary representation (14.4f32 becomes 14.399999618530273 in the file), so go
/// through the shortest decimal that reads back as the same `f32`.
fn f32_value(v: f32) -> Value {
    format!("{v:?}").parse::<f64>().unwrap_or(v as f64).into()
}

/// Set `[section] key = value`, adding the table or the key when either is
/// missing. Replacing an existing value keeps its decor, so only the value
/// itself changes on disk - the surrounding spacing and any trailing comment
/// stay put.
fn set(doc: &mut DocumentMut, section: &str, key: &str, value: Value) {
    let table = doc
        .entry(section)
        .or_insert_with(|| Item::Table(Table::new()));
    let Some(table) = table.as_table_mut() else {
        return;
    };
    let mut value = value;
    if let Some(old) = table.get(key).and_then(Item::as_value) {
        *value.decor_mut() = old.decor().clone();
    } else {
        *value.decor_mut() = toml_edit::Decor::new(" ", "");
    }
    table.insert(key, Item::Value(value));
}

/// Rewrite `[ble.names]` to match `names`, adding the sub-table when it is
/// missing. Entries that are still present keep their decor (so a comment on a
/// nickname line survives), entries no longer in the map are dropped - that is
/// what makes "forget this board" stick across a save.
///
/// MAC keys need quoting, which `toml_edit` applies by itself: a bare key
/// cannot contain colons, so it falls back to a quoted one.
fn set_names(doc: &mut DocumentMut, names: &BTreeMap<String, String>) {
    let table = doc
        .entry("ble")
        .or_insert_with(|| Item::Table(Table::new()));
    let Some(table) = table.as_table_mut() else {
        return;
    };
    let names_table = table
        .entry("names")
        .or_insert_with(|| Item::Table(Table::new()));
    let Some(names_table) = names_table.as_table_mut() else {
        return;
    };
    // Drop keys we no longer know, and any spelled differently from the
    // canonical form - the loop below re-adds those under their normalized
    // key, and keeping the old spelling would file the same board twice.
    names_table.retain(|key, _| names.contains_key(key) && normalize_mac(key) == key);
    for (mac, name) in names {
        if let Some(old) = names_table.get(mac).and_then(Item::as_value) {
            let mut value: Value = name.as_str().into();
            *value.decor_mut() = old.decor().clone();
            names_table.insert(mac, Item::Value(value));
        } else {
            names_table.insert(mac, Item::Value(name.as_str().into()));
        }
    }
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

    /// Write these settings to the TOML file at `path`, returning `true` when
    /// the file had to be created.
    ///
    /// An existing file is edited in place rather than rewritten: comments, key
    /// order, and any keys this app knows nothing about all survive, and only
    /// the values it owns are replaced. With no file there (or an unreadable
    /// one) it generates a fresh documented one from [`Self::to_toml`].
    pub fn save(&self, path: &str) -> Result<bool, String> {
        let Ok(text) = std::fs::read_to_string(path) else {
            std::fs::write(path, self.to_toml()).map_err(|e| format!("{path}: {e}"))?;
            return Ok(true);
        };
        let mut doc: DocumentMut = text.parse().map_err(|e| format!("{path}: {e}"))?;
        set(&mut doc, "colors", "track", hex(self.colors.track).into());
        set(&mut doc, "colors", "fixed", hex(self.colors.fixed).into());
        set(&mut doc, "sizes", "marker", f32_value(self.sizes.marker));
        set(&mut doc, "sizes", "beacon", f32_value(self.sizes.beacon));
        set(&mut doc, "sizes", "track", f32_value(self.sizes.track));
        set(
            &mut doc,
            "sizes",
            "distance_line",
            f32_value(self.sizes.distance_line),
        );
        set(
            &mut doc,
            "sizes",
            "distance_text",
            f32_value(self.sizes.distance_text),
        );
        set(&mut doc, "distance", "show", self.distance.show.into());
        set(
            &mut doc,
            "distance",
            "units",
            self.distance.units.as_str().into(),
        );
        set(&mut doc, "distance", "dotted", self.distance.dotted.into());
        set(&mut doc, "ble", "enabled", self.ble.enabled.into());
        set(&mut doc, "ble", "show_path", self.ble.show_path.into());
        // An empty string reads back as "unset", so an unpinned MAC keeps the
        // key in the file rather than dropping the line.
        set(
            &mut doc,
            "ble",
            "mac",
            self.ble.mac.clone().unwrap_or_default().into(),
        );
        set(
            &mut doc,
            "track",
            "min_distance",
            self.track.min_distance.into(),
        );
        set_names(&mut doc, &self.ble.names);
        std::fs::write(path, doc.to_string()).map_err(|e| format!("{path}: {e}"))?;
        Ok(false)
    }

    /// These settings as a complete, commented TOML file - what the Settings
    /// page generates when there is no config file yet.
    pub fn to_toml(&self) -> String {
        let s = &self.sizes;
        // With no boards named yet, show the shape as a comment: the section is
        // hand-editable, and an empty header explains nothing about what goes
        // in it.
        let names = if self.ble.names.is_empty() {
            "# \"AA:BB:CC:DD:EE:FF\" = \"Truck\"\n".to_string()
        } else {
            self.ble
                .names
                .iter()
                .map(|(mac, name)| format!("{mac:?} = {name:?}\n"))
                .collect()
        };
        format!(
            "# gps-gui-rs settings. Every key is optional; a missing one keeps its default.\n\
             \n\
             [colors]\n\
             track = \"{track}\"   # phone track, heading arrow, position dot\n\
             fixed = \"{fixed}\"   # BLE beacon marker, distance line, beacon path\n\
             \n\
             [sizes]              # screen points; each overlay is sized independently\n\
             marker = {marker:?}          # current-position dot radius\n\
             beacon = {beacon:?}          # beacon dot radius\n\
             track = {track_w:?}           # track polyline width (phone track and beacon path)\n\
             distance_line = {dline:?}    # user<->beacon line width\n\
             distance_text = {dtext:?}   # beacon-distance label font size\n\
             \n\
             [distance]\n\
             show = {show}        # draw the distance label on the line to the beacon\n\
             units = \"{units}\"    # \"metric\" (km/m) or \"imperial\" (mi/ft)\n\
             dotted = {dotted}       # draw distance line dotted rather than solid\n\
             \n\
             [ble]\n\
             enabled = {enabled}       # master switch for the BLE GPS source\n\
             show_path = {show_path}    # draw the path of the incoming BLE GPS data\n\
             mac = \"{mac}\"            # pin a specific device; empty scans by service\n\
             \n\
             [ble.names]          # nicknames for known boards; they all advertise the same name\n\
             {names}\
             \n\
             [track]\n\
             min_distance = {min_distance:?}   # meters of movement before a new track point\n",
            track = hex(self.colors.track),
            fixed = hex(self.colors.fixed),
            marker = s.marker,
            beacon = s.beacon,
            track_w = s.track,
            dline = s.distance_line,
            dtext = s.distance_text,
            show = self.distance.show,
            units = self.distance.units.as_str(),
            dotted = self.distance.dotted,
            enabled = self.ble.enabled,
            show_path = self.ble.show_path,
            mac = self.ble.mac.clone().unwrap_or_default(),
            names = names,
            min_distance = self.track.min_distance,
        )
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
        // Normalize on the way in: a hand-edited file may spell a MAC any way,
        // and the picker looks these up by normalized key.
        config.ble.names = raw
            .ble
            .names
            .into_iter()
            .filter(|(_, name)| !name.trim().is_empty())
            .map(|(mac, name)| (normalize_mac(&mac), name.trim().to_string()))
            .collect();
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
    fn generated_file_reads_back_as_the_same_settings() {
        let cfg = AppConfig::default();
        let back = AppConfig::from_toml(&cfg.to_toml()).unwrap();
        assert_eq!(back.colors.fixed, cfg.colors.fixed);
        assert_eq!(back.sizes.distance_text, cfg.sizes.distance_text);
        assert_eq!(back.distance.units, cfg.distance.units);
        assert_eq!(back.track.min_distance, cfg.track.min_distance);
        // The generated `mac = ""` means "scan by service", not a pinned MAC.
        assert_eq!(back.ble.mac, None);
    }

    #[test]
    fn save_edits_in_place_and_round_trips() {
        let path = std::env::temp_dir().join("gps-gui-rs-config-save-test.toml");
        let path = path.to_str().unwrap();
        std::fs::write(
            path,
            "# keep me\n[sizes]\nmarker = 8.0 # and me\n\n[extra]\nunknown = 1\n",
        )
        .unwrap();

        let mut cfg = AppConfig::default();
        cfg.sizes.marker = 12.5;
        cfg.colors.track = Color32::from_rgb(1, 2, 3);
        // False: the file was already there, so it was edited, not generated.
        assert!(!cfg.save(path).unwrap());

        let text = std::fs::read_to_string(path).unwrap();
        assert!(text.contains("# keep me"), "{text}");
        assert!(text.contains("# and me"), "{text}");
        assert!(text.contains("unknown = 1"), "{text}");

        let back = AppConfig::load(path).unwrap();
        assert_eq!(back.sizes.marker, 12.5);
        assert_eq!(back.colors.track, Color32::from_rgb(1, 2, 3));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn mac_normalization_folds_case_and_separators() {
        assert_eq!(normalize_mac("aa:bb:cc:dd:ee:ff"), "AA:BB:CC:DD:EE:FF");
        assert_eq!(normalize_mac("AA-BB-CC-DD-EE-FF"), "AA:BB:CC:DD:EE:FF");
        assert_eq!(normalize_mac(" aabbccddeeff "), "AA:BB:CC:DD:EE:FF");
    }

    #[test]
    fn names_are_found_however_the_mac_is_spelled() {
        let cfg = AppConfig::from_toml(
            "[ble]\nmac = \"aa:bb:cc:dd:ee:ff\"\n\n[ble.names]\n\"AA-BB-CC-DD-EE-FF\" = \"Truck\"\n",
        )
        .unwrap();
        // Stored canonically regardless of how the file spelled it.
        assert_eq!(cfg.ble.names.get("AA:BB:CC:DD:EE:FF").unwrap(), "Truck");
        assert_eq!(cfg.ble.name_of("aa:bb:cc:dd:ee:ff"), Some("Truck"));
        // The pinned MAC matches the same board despite the different spelling.
        assert!(cfg.ble.is_selected("AA-BB-CC-DD-EE-FF"));
        assert!(!cfg.ble.is_selected("11:22:33:44:55:66"));
    }

    #[test]
    fn unnamed_board_falls_back_to_its_mac() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.ble.label_of("AA:BB:CC:DD:EE:FF"), "AA:BB:CC:DD:EE:FF");
    }

    #[test]
    fn set_name_adds_renames_and_forgets() {
        let mut ble = BleSettings::default();
        ble.set_name("aa:bb:cc:dd:ee:ff", "Truck");
        assert_eq!(ble.name_of("AA:BB:CC:DD:EE:FF"), Some("Truck"));
        // Renaming through a different spelling hits the same entry.
        ble.set_name("AA-BB-CC-DD-EE-FF", "Van");
        assert_eq!(ble.names.len(), 1);
        assert_eq!(ble.name_of("AA:BB:CC:DD:EE:FF"), Some("Van"));
        // Blanking the name forgets the board.
        ble.set_name("AA:BB:CC:DD:EE:FF", "  ");
        assert!(ble.names.is_empty());
    }

    #[test]
    fn saved_names_round_trip_and_forgetting_sticks() {
        let path = std::env::temp_dir().join("gps-gui-rs-config-names-test.toml");
        let path = path.to_str().unwrap();
        let _ = std::fs::remove_file(path);

        let mut cfg = AppConfig::default();
        cfg.ble.set_name("AA:BB:CC:DD:EE:FF", "Truck");
        cfg.ble.set_name("11:22:33:44:55:66", "Backpack");
        // No file yet, so this generates one from the template.
        assert!(cfg.save(path).unwrap());
        let back = AppConfig::load(path).unwrap();
        assert_eq!(back.ble.name_of("AA:BB:CC:DD:EE:FF"), Some("Truck"));
        assert_eq!(back.ble.name_of("11:22:33:44:55:66"), Some("Backpack"));

        // Forget one and save over the existing file: the line has to go, not
        // linger because the edit-in-place path only ever adds.
        let mut cfg = back;
        cfg.ble.set_name("11:22:33:44:55:66", "");
        assert!(!cfg.save(path).unwrap());
        let back = AppConfig::load(path).unwrap();
        assert_eq!(back.ble.name_of("AA:BB:CC:DD:EE:FF"), Some("Truck"));
        assert_eq!(back.ble.name_of("11:22:33:44:55:66"), None);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn sizes_reject_non_positive() {
        assert!(super::AppConfig::from_toml("[sizes]\nmarker = 0.0").is_err());
        assert!(super::AppConfig::from_toml("[sizes]\nbeacon = -1.0").is_err());
        let cfg = super::AppConfig::from_toml("[sizes]\nmarker = 12.0").unwrap();
        assert_eq!(cfg.sizes.marker, 12.0);
    }
}
