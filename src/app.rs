use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::SystemTime;

use egui::Pos2;
use gps_proto::packet::{self, PositionPacket};
use walkers::{
    lat_lon, sources::OpenStreetMap, HeaderValue, HttpOptions, HttpTiles, MapMemory, Position,
};

use crate::ble::{BleCommand, BleEvent, BleHandle, Telemetry};
use crate::config::AppConfig;
use crate::gps::GpsFix;
use crate::offline::{self, DownloadProgress};
use crate::points::{PointSource, TrackPoint};

/// The view layer (page rendering + shared egui scaffolding). Kept in a
/// submodule so this file holds only state and the core update logic; the
/// `impl MyApp` blocks there render each page.
mod ui;

/// Great-circle distance between two positions in meters (haversine formula).
fn haversine_m(a: Position, b: Position) -> f64 {
    const EARTH_RADIUS_M: f64 = 6_371_000.0;
    let lat1 = a.y().to_radians();
    let lat2 = b.y().to_radians();
    let dlat = (b.y() - a.y()).to_radians();
    let dlon = (b.x() - a.x()).to_radians();
    let h = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlon / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_M * h.sqrt().asin()
}

/// Whether `pos` is far enough from the last recorded track point to append it.
/// Always true for the first point; otherwise the move must be at least
/// `min_distance_m`, so a track is decimated to points that far apart.
fn far_enough(last: Option<&Position>, pos: Position, min_distance_m: f64) -> bool {
    match last {
        None => true,
        Some(&last) => haversine_m(last, pos) >= min_distance_m,
    }
}

/// Which screen is shown. The page menu switches between them.
#[derive(Clone, Copy, PartialEq)]
pub enum Page {
    /// The interactive map with the position marker and track.
    Map,
    /// A plain read-out of the current latitude and longitude.
    Data,
    /// Searchable list of all recorded GPS points.
    Points,
    /// Board health for the esp32c6-gps board (ESP/WIO/GPS/LoRa).
    Status,
    /// Loading the TOML config file (marker colors, BLE beacon).
    Settings,
}

/// Box-selection state for the offline region download on the map page.
#[derive(Clone, Copy)]
pub enum RegionSelect {
    /// Not selecting; the map behaves normally.
    Inactive,
    /// Waiting for / tracking the box drag. Panning is disabled so the drag
    /// draws a box instead of moving the map.
    Picking {
        start: Option<Pos2>,
        current: Option<Pos2>,
    },
    /// Box chosen; the confirm panel is shown over the map.
    Confirm {
        a: Position,
        b: Position,
        max_zoom: u8,
    },
}

/// Source filter on the points page.
#[derive(Clone, Copy, PartialEq)]
pub enum PointFilter {
    All,
    Phone,
    Esp,
}

impl PointFilter {
    fn admits(self, source: PointSource) -> bool {
        match self {
            PointFilter::All => true,
            PointFilter::Phone => source == PointSource::Phone,
            PointFilter::Esp => source == PointSource::Esp,
        }
    }
}

/// A map marker the user can double-click/tap to inspect.
#[derive(Clone, Copy, PartialEq)]
pub enum MarkerKind {
    /// The phone / manual position dot.
    You,
    /// The BLE GPS beacon.
    Beacon,
}

impl MarkerKind {
    fn label(self) -> &'static str {
        match self {
            MarkerKind::You => "You",
            MarkerKind::Beacon => "Beacon",
        }
    }
}

/// HTTP tile options caching to `cache_dir` (when writable). Tiles fetched once
/// are reused from disk, so previously viewed areas keep working without a
/// network. `None` disables the cache. The user agent matches the offline
/// downloader's so both read and write the same cache entries.
fn http_options(cache_dir: Option<PathBuf>) -> HttpOptions {
    HttpOptions {
        cache: cache_dir,
        user_agent: Some(HeaderValue::from_static(offline::USER_AGENT)),
        ..Default::default()
    }
}

pub struct MyApp {
    tiles: HttpTiles,
    map_memory: MapMemory,
    /// Live GPS fixes, when a source is wired up (Android GNSS). `None` on
    /// desktop, where the manual position bar is shown instead.
    gps_rx: Option<Receiver<GpsFix>>,
    /// Optional device-facing compass heading stream (Android only).
    compass_rx: Option<Receiver<f32>>,
    /// The BLE worker streaming the ESP32-C3 beacon's GPS data.
    ble: BleHandle,
    /// Returns the current safe-area insets `[top, right, bottom, left]` in
    /// physical pixels. `None` on desktop (no system bars to avoid).
    insets: Option<Box<dyn Fn() -> [f32; 4]>>,
    current: Option<Position>,
    /// When the current position was last updated, for the marker info popup.
    current_time: Option<SystemTime>,
    /// Course over ground from the GPS fix.
    heading: Option<f32>,
    /// Device-facing heading from the compass sensor.
    compass_heading: Option<f32>,
    /// When set, the map is rotated so the current heading points up.
    heading_up: bool,
    /// Rotation angle actually drawn, eased toward the live heading each frame so
    /// the map turns smoothly instead of snapping between sensor readings.
    smoothed_heading: Option<f32>,
    track: Vec<TrackPoint>,
    /// Live position of the BLE beacon; replaces the old fixed reference
    /// point, so the distance line tracks the real device.
    beacon: Option<Position>,
    /// When the beacon position was last updated, for the marker info popup.
    beacon_time: Option<SystemTime>,
    /// The last full packet from the beacon (satellites, speed, ...).
    beacon_packet: Option<PositionPacket>,
    /// Every beacon position recorded, for the path drawing and points list.
    beacon_track: Vec<TrackPoint>,
    /// Draw the beacon path (TOML `[ble] show_path`, also togglable in
    /// Settings).
    show_beacon_path: bool,
    /// Last BLE status line, for the Settings page.
    ble_status: String,
    ble_connected: bool,
    /// Notify-interval input on the Settings page.
    ble_interval_text: String,
    /// Result of the last config write: device ack (green) or error (red).
    ble_ack: Option<Result<String, String>>,
    /// A config write is in flight and the ack has not arrived yet.
    ble_ack_pending: bool,
    /// Latest board telemetry (esp32c6-gps), for the Status page.
    telemetry: Option<Telemetry>,
    /// Latest WIO status/log line relayed by the board.
    board_log: Option<String>,
    /// Which screen is currently shown.
    page: Page,
    /// Loaded configuration (marker colors, BLE settings).
    config: AppConfig,
    /// The config-file path typed on the Settings page.
    config_path: String,
    /// Result of the last load attempt: `Ok` message (green) or error (red).
    config_feedback: Option<Result<String, String>>,
    /// Tile cache directory; also the target of offline region downloads.
    cache_dir: Option<PathBuf>,
    /// Box-selection state for the offline region download.
    select: RegionSelect,
    /// Progress of the running (or just-finished) offline tile download.
    download: Option<Arc<DownloadProgress>>,
    /// Search query on the points page.
    points_search: String,
    /// Source filter on the points page.
    points_filter: PointFilter,
    /// Text in the manual position bar (shown only when `gps_rx` is `None`).
    manual_gps_text: String,
    /// The last manual position entry failed to parse.
    manual_gps_bad: bool,
    /// Marker whose info popup (name + time since last update) is shown, set by
    /// double-clicking/tapping a marker on the map.
    selected_marker: Option<MarkerKind>,
    /// Offline center-button zoom fallback: the background probe sends the zoom
    /// level to snap to (nearest cached tile) here when it finds we are offline.
    zoom_tx: Sender<f64>,
    zoom_rx: Receiver<f64>,
}

impl MyApp {
    /// `gps_rx` is the live GPS fix stream, or `None` when no source is wired
    /// up (desktop) - the UI then shows a manual position entry bar instead.
    /// `cache_dir` is where tiles are cached to disk (`None` to disable). Desktop
    /// passes a local `.cache`; Android passes its writable data directory.
    /// `compass_rx` is the device-facing heading stream (`None` on desktop).
    /// `insets` reports the safe-area insets in physical pixels (`None` on desktop).
    /// `ble` is the worker connected to the ESP32-C3 GPS beacon.
    pub fn new(
        ctx: egui::Context,
        gps_rx: Option<Receiver<GpsFix>>,
        cache_dir: Option<PathBuf>,
        compass_rx: Option<Receiver<f32>>,
        insets: Option<Box<dyn Fn() -> [f32; 4]>>,
        ble: BleHandle,
    ) -> Self {
        // SVG loader for the button icons.
        egui_extras::install_image_loaders(&ctx);

        let (zoom_tx, zoom_rx) = std::sync::mpsc::channel();

        let mut app = Self {
            tiles: HttpTiles::with_options(OpenStreetMap, http_options(cache_dir.clone()), ctx),
            map_memory: MapMemory::default(),
            gps_rx,
            compass_rx,
            ble,
            insets,
            current: None,
            current_time: None,
            heading: None,
            compass_heading: None,
            heading_up: false,
            smoothed_heading: None,
            track: Vec::new(),
            beacon: None,
            beacon_time: None,
            beacon_packet: None,
            beacon_track: Vec::new(),
            show_beacon_path: false,
            ble_status: "idle".to_string(),
            ble_connected: false,
            ble_interval_text: packet::UPDATE_INTERVAL_DEFAULT_MS.to_string(),
            ble_ack: None,
            ble_ack_pending: false,
            telemetry: None,
            board_log: None,
            page: Page::Map,
            config: AppConfig::default(),
            config_path: String::new(),
            config_feedback: None,
            cache_dir,
            select: RegionSelect::Inactive,
            download: None,
            points_search: String::new(),
            points_filter: PointFilter::All,
            manual_gps_text: String::new(),
            manual_gps_bad: false,
            selected_marker: None,
            zoom_tx,
            zoom_rx,
        };

        // Auto-load ./gps-config.toml when present (desktop convenience); the
        // Settings page can load any path later. With no file the defaults
        // apply, which include connecting to the beacon.
        match AppConfig::load("gps-config.toml") {
            Ok(cfg) => app.apply_config(cfg),
            Err(_) => app.sync_ble_to_config(),
        }
        app
    }

    /// Adopt a loaded config: colors, path toggle, and the BLE connection.
    fn apply_config(&mut self, cfg: AppConfig) {
        self.show_beacon_path = cfg.ble.show_path;
        self.config = cfg;
        self.sync_ble_to_config();
    }

    /// Tell the BLE worker what the config wants (connect or stay away).
    fn sync_ble_to_config(&mut self) {
        let cmd = if self.config.ble.enabled {
            BleCommand::Connect {
                mac: self.config.ble.mac.clone(),
            }
        } else {
            BleCommand::Disconnect
        };
        let _ = self.ble.commands.send(cmd);
    }

    /// Load the config file at `config_path`, recording a human-readable
    /// result for the Settings page to show.
    fn load_config(&mut self) {
        let path = self.config_path.trim().to_string();
        if path.is_empty() {
            self.config_feedback = Some(Err("Enter a file path.".to_string()));
            return;
        }
        self.config_feedback = Some(match AppConfig::load(&path) {
            Ok(cfg) => {
                self.apply_config(cfg);
                Ok(format!("Loaded {path}"))
            }
            Err(e) => Err(e),
        });
    }

    /// Safe-area inset at the top (status bar) in egui points.
    fn top_inset(&self, ctx: &egui::Context) -> f32 {
        match &self.insets {
            Some(f) => f()[0] / ctx.pixels_per_point(),
            None => 0.0,
        }
    }

    /// Safe-area inset at the bottom (gesture bar) in egui points.
    fn bottom_inset(&self, ctx: &egui::Context) -> f32 {
        match &self.insets {
            Some(f) => f()[2] / ctx.pixels_per_point(),
            None => 0.0,
        }
    }

    /// Device-facing compass heading if available, otherwise course over ground.
    fn effective_heading(&self) -> Option<f32> {
        self.compass_heading.or(self.heading)
    }

    /// Great-circle distance from the current position to the BLE beacon, in
    /// meters. `None` until both a fix and a beacon position are known.
    fn distance_to_beacon(&self) -> Option<f64> {
        match (self.current, self.beacon) {
            (Some(cur), Some(beacon)) => Some(haversine_m(cur, beacon)),
            _ => None,
        }
    }

    /// Apply one phone/manual GPS fix: move the marker, update the heading, and
    /// append to the recorded track (decimated by the min-distance setting).
    fn apply_gps_fix(&mut self, fix: GpsFix) {
        let pos = lat_lon(fix.lat, fix.lon);
        self.current = Some(pos);
        self.current_time = Some(SystemTime::now());
        self.heading = fix.bearing;
        if far_enough(
            self.track.last().map(|t| &t.pos),
            pos,
            self.config.track.min_distance,
        ) {
            self.track.push(TrackPoint {
                pos,
                source: PointSource::Phone,
                time: SystemTime::now(),
            });
        }
    }

    /// Pull every pending fix out of the channels, updating the current
    /// position, the beacon, and their tracks.
    fn drain_sources(&mut self) {
        while let Some(fix) = self.gps_rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.apply_gps_fix(fix);
        }

        if let Some(rx) = &self.compass_rx {
            while let Ok(heading) = rx.try_recv() {
                self.compass_heading = Some(heading);
            }
        }

        while let Ok(event) = self.ble.events.try_recv() {
            match event {
                BleEvent::Status(s) => self.ble_status = s,
                BleEvent::Connected(c) => self.ble_connected = c,
                BleEvent::Fix(p) => {
                    self.beacon_packet = Some(p);
                    if p.has_fix() {
                        let pos = lat_lon(p.lat_deg(), p.lon_deg());
                        self.beacon = Some(pos);
                        self.beacon_time = Some(SystemTime::now());
                        if far_enough(
                            self.beacon_track.last().map(|t| &t.pos),
                            pos,
                            self.config.track.min_distance,
                        ) {
                            self.beacon_track.push(TrackPoint {
                                pos,
                                source: PointSource::Esp,
                                time: SystemTime::now(),
                            });
                        }
                    }
                }
                BleEvent::Ack(ack) => {
                    self.ble_ack_pending = false;
                    self.ble_ack = Some(match ack.status {
                        packet::ACK_OK => Ok(format!(
                            "Device applied: interval {} ms",
                            ack.value_u32.unwrap_or(0)
                        )),
                        packet::ACK_UNKNOWN_ID => {
                            Err("Device rejected: unknown setting".to_string())
                        }
                        _ => Err("Device rejected: bad value".to_string()),
                    });
                }
                BleEvent::Telemetry(t) => self.telemetry = Some(t),
                BleEvent::Log(s) => self.board_log = Some(s),
            }
        }

        // Offline center-button fallback: apply the zoom the probe picked (the
        // nearest level with a cached tile). Latest wins if several arrived.
        while let Ok(zoom) = self.zoom_rx.try_recv() {
            let _ = self.map_memory.set_zoom(zoom);
        }
    }
}

impl eframe::App for MyApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain_sources();

        let ctx = ui.ctx().clone();
        let screen = ctx.input(|i| i.viewport_rect());

        match self.page {
            Page::Map => self.map_page(&ctx, screen),
            Page::Data => self.data_page(&ctx, screen),
            Page::Points => self.points_page(&ctx, screen),
            Page::Status => self.status_page(&ctx, screen),
            Page::Settings => self.settings_page(&ctx, screen),
        }

        // Every page but the map gets the floating corner toggle; on the map
        // page the toggle lives at the right end of the controls bar instead.
        if !matches!(self.page, Page::Map) {
            self.page_toggle(&ctx, screen);
        }
        // Offline download progress floats above every page too.
        self.download_ui(&ctx, screen);

        // With no live GPS source (desktop), let a position be typed in. Shown
        // on the position-facing pages; the bar floats at the bottom.
        if self.gps_rx.is_none() && matches!(self.page, Page::Map | Page::Data) {
            self.manual_gps_bar(&ctx, screen);
        }
    }
}
