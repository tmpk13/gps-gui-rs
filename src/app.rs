use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::SystemTime;

use egui::emath::Rot2;
use egui::{Pos2, Shape};
use gps_proto::packet::{self, PositionPacket};
use walkers::{
    lat_lon, sources::OpenStreetMap, HeaderValue, HttpOptions, HttpTiles, Map, MapMemory,
    Position, Projector,
};

use crate::ble::{BleCommand, BleEvent, BleHandle, Telemetry};
use midair_proto::link::{
    TELEM_FLAG_CFG_LOADED, TELEM_FLAG_GPS_FIX, TELEM_FLAG_SD_OK,
};
use crate::config::AppConfig;
use crate::gps::GpsFix;
use crate::marker::GpsLayer;
use crate::offline::{self, DownloadProgress};
use crate::points::{age_text, PointSource, TrackPoint};

/// Where the map looks before the first GPS fix arrives.
fn default_position() -> Position {
    lat_lon(54.333, -122.676)
}

/// Icon side length as a fraction of the smaller screen dimension, clamped to
/// this point range. Keeps the toolbar proportional across phone and desktop.
const ICON_SIZE_FRAC: f32 = 0.05;
const ICON_SIZE_MIN: f32 = 20.0;
const ICON_SIZE_MAX: f32 = 40.0;

/// Inset of the floating corner toggle from the screen edge, as a fraction of
/// the smaller screen dimension.
const CORNER_MARGIN_FRAC: f32 = 0.03;

/// Square icon side length in points for the current screen size.
fn icon_size_for(screen: egui::Rect) -> f32 {
    (screen.size().min_elem() * ICON_SIZE_FRAC).clamp(ICON_SIZE_MIN, ICON_SIZE_MAX)
}

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

/// Format a distance given in meters: kilometers once it is at least 1 km,
/// otherwise whole meters.
fn format_distance(m: f64) -> String {
    if m >= 1000.0 {
        format!("{:.2} km", m / 1000.0)
    } else {
        format!("{m:.0} m")
    }
}

/// A square icon button. The icons are white SVGs tinted to the current text
/// color so they follow the theme.
fn icon_button(ui: &mut egui::Ui, size: f32, source: egui::ImageSource<'_>) -> egui::Response {
    icon_button_pulse(ui, size, source, false)
}

/// Same as [`icon_button`], but when `pulse` is set the button background
/// oscillates red to flag that the action currently has no target (used by the
/// center button when there is no marker to center on).
fn icon_button_pulse(
    ui: &mut egui::Ui,
    size: f32,
    source: egui::ImageSource<'_>,
    pulse: bool,
) -> egui::Response {
    let tint = ui.visuals().text_color();
    let mut button = egui::Button::image(
        egui::Image::new(source)
            .fit_to_exact_size(egui::vec2(size, size))
            .tint(tint),
    );
    if pulse {
        // 0..1 oscillation, one cycle every ~1.6s.
        let t = ui.input(|i| i.time);
        let wave = 0.5 + 0.5 * (t * std::f64::consts::PI * 1.25).sin() as f32;
        let alpha = (60.0 + wave * 150.0) as u8;
        button = button.fill(egui::Color32::from_rgba_unmultiplied(200, 40, 40, alpha));
        // Keep the animation running even when nothing else asks for a repaint.
        ui.ctx().request_repaint();
    }
    ui.add(button)
}

/// Which screen is shown. The corner toggle switches between them.
#[derive(Clone, Copy, PartialEq)]
enum Page {
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

/// Every page in menu order, each with its label and icon. Drives the page
/// dropdown menu.
fn page_items() -> [(Page, &'static str, egui::ImageSource<'static>); 5] {
    [
        (
            Page::Map,
            "Map",
            egui::include_image!("../assets/icons/map.svg"),
        ),
        (
            Page::Data,
            "Data",
            egui::include_image!("../assets/icons/data.svg"),
        ),
        (
            Page::Points,
            "Points",
            egui::include_image!("../assets/icons/points.svg"),
        ),
        (
            Page::Status,
            "Status",
            egui::include_image!("../assets/icons/status.svg"),
        ),
        (
            Page::Settings,
            "Settings",
            egui::include_image!("../assets/icons/settings.svg"),
        ),
    ]
}

/// Refuse offline downloads bigger than this many tiles (tile-server
/// courtesy; shrink the box or lower the max zoom instead).
const MAX_REGION_TILES: u64 = 10_000;

/// Rough average size of a cached OSM tile, for the download estimate.
const TILE_SIZE_ESTIMATE_KB: u64 = 15;

/// Box-selection state for the offline region download on the map page.
#[derive(Clone, Copy)]
enum RegionSelect {
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
enum PointFilter {
    All,
    Phone,
    Esp,
}

/// A map marker the user can double-click/tap to inspect.
#[derive(Clone, Copy, PartialEq)]
enum MarkerKind {
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

/// How close (in points) a double-click must land to a marker to select it.
const MARKER_HIT_RADIUS: f32 = 24.0;

impl PointFilter {
    fn admits(self, source: PointSource) -> bool {
        match self {
            PointFilter::All => true,
            PointFilter::Phone => source == PointSource::Phone,
            PointFilter::Esp => source == PointSource::Esp,
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
    }

    fn controls(&mut self, ui: &mut egui::Ui, screen: egui::Rect) {
        let icon = icon_size_for(screen);
        ui.spacing_mut().button_padding = egui::vec2(icon * 0.7, icon * 0.45);
        // Center the row in the full-width bar so the leftover space is even on
        // both sides.
        ui.vertical_centered(|ui| {
            ui.horizontal(|ui| {
                // Center on the user marker if we have a fix; otherwise fall back
                // to the next available marker (the beacon). With no marker at
                // all the button pulses red and does nothing when clicked.
                let has_marker = self.current.is_some() || self.beacon.is_some();
                if icon_button_pulse(
                    ui,
                    icon,
                    egui::include_image!("../assets/icons/center.svg"),
                    !has_marker,
                )
                .on_hover_text("Center on position")
                .clicked()
                {
                    if self.current.is_some() {
                        self.map_memory.follow_my_position();
                    } else if let Some(pos) = self.beacon {
                        self.map_memory.center_at(pos);
                    }
                }

                // Shows the mode the button switches TO (like the old label).
                let (rotate_icon, rotate_hint) = if self.heading_up {
                    (
                        egui::include_image!("../assets/icons/north.svg"),
                        "North up",
                    )
                } else {
                    (
                        egui::include_image!("../assets/icons/heading.svg"),
                        "Heading up",
                    )
                };
                if icon_button(ui, icon, rotate_icon)
                    .on_hover_text(rotate_hint)
                    .clicked()
                {
                    self.heading_up = !self.heading_up;
                }

                if icon_button(ui, icon, egui::include_image!("../assets/icons/zoom-in.svg"))
                    .on_hover_text("Zoom in")
                    .clicked()
                {
                    let _ = self.map_memory.zoom_in();
                }
                if icon_button(ui, icon, egui::include_image!("../assets/icons/zoom-out.svg"))
                    .on_hover_text("Zoom out")
                    .clicked()
                {
                    let _ = self.map_memory.zoom_out();
                }
                if icon_button(ui, icon, egui::include_image!("../assets/icons/clear.svg"))
                    .on_hover_text("Clear tracks")
                    .clicked()
                {
                    self.track.clear();
                    self.beacon_track.clear();
                }

                // Offline region download: only when tiles are cached to disk,
                // and one download at a time.
                if self.cache_dir.is_some() && self.download.is_none() {
                    let selecting = !matches!(self.select, RegionSelect::Inactive);
                    let hint = if selecting {
                        "Cancel region selection"
                    } else {
                        "Download region"
                    };
                    if icon_button(ui, icon, egui::include_image!("../assets/icons/download.svg"))
                        .on_hover_text(hint)
                        .clicked()
                    {
                        self.select = if selecting {
                            RegionSelect::Inactive
                        } else {
                            RegionSelect::Picking {
                                start: None,
                                current: None,
                            }
                        };
                    }
                }

                // The page menu sits inline, right after the other buttons.
                self.page_menu(ui, icon);
            });
        });
    }

    /// Paint the map into `map_rect` (which may overscan past `clip`). When
    /// `rotation` is set, the painted shapes are rotated about the center of
    /// `clip` and then clipped back to `clip`, so the visible map spins with the
    /// heading while its corners stay filled by the overscan.
    fn map(
        &mut self,
        ui: &mut egui::Ui,
        map_rect: egui::Rect,
        rotation: Option<Rot2>,
        clip: egui::Rect,
    ) {
        let my_position = self.current.unwrap_or_else(default_position);

        let layer = GpsLayer {
            current: self.current,
            track: self.track.iter().map(|t| t.pos).collect(),
            heading: self.effective_heading(),
            beacon: self.beacon,
            beacon_track: if self.show_beacon_path {
                self.beacon_track.iter().map(|t| t.pos).collect()
            } else {
                Vec::new()
            },
            show_beacon_path: self.show_beacon_path,
            colors: self.config.colors,
        };

        // walkers sizes itself to the child's available space, so give it the
        // (possibly overscanned) map rect.
        let layer_id = ui.layer_id();
        let start = ui.ctx().graphics_mut(|g| g.entry(layer_id).next_idx());

        let android = cfg!(target_os = "android");
        // Heading-up on mobile locks the view (centered, no pan/zoom gesture);
        // the zoom buttons still work. North-up keeps normal pan.
        let locked = rotation.is_some() && android;

        // The map is drawn inside a background Area (rotation overscan on
        // Android, full-bleed on desktop). walkers' built-in zoom only fires
        // when the map is the top interactable layer under the pointer, which a
        // background Area never is, so its scroll/pinch zoom silently no-ops. We
        // drive zoom ourselves here and turn walkers' own gesture off below.
        let pinching;

        #[cfg(target_os = "android")]
        {
            // Pinch: mirror walkers' own `zoom_by((delta - 1) * zoom_speed)`.
            let zoom_delta = ui.ctx().input(|i| i.zoom_delta());
            pinching = (zoom_delta - 1.0).abs() > 0.001;
            if pinching {
                let zoom = self.map_memory.zoom() + (zoom_delta as f64 - 1.0) * 2.0;
                let _ = self.map_memory.set_zoom(zoom);
            }
        }

        #[cfg(not(target_os = "android"))]
        {
            pinching = false;
            // Bare mouse-wheel zoom about the map center (like the +/- buttons),
            // gated on the pointer being over the map rect rather than walkers'
            // layer-topmost check (which the background Area fails).
            let (scroll_y, hover) =
                ui.ctx().input(|i| (i.smooth_scroll_delta.y, i.pointer.hover_pos()));
            if scroll_y != 0.0 && hover.is_some_and(|p| clip.contains(p)) {
                let zoom = self.map_memory.zoom() + scroll_y as f64 * 0.005;
                let _ = self.map_memory.set_zoom(zoom);
                ui.ctx().request_repaint();
            }
        }

        // Suppress pan while pinching so the two-finger gesture zooms instead of
        // dragging (walkers normally keeps zoom and pan mutually exclusive).
        // While a download box is being picked, the drag draws the box instead.
        let picking = matches!(self.select, RegionSelect::Picking { .. });
        let allow_pan = !locked && !pinching && !picking;

        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(map_rect));
        let map = Map::new(Some(&mut self.tiles), &mut self.map_memory, my_position)
            .with_plugin(layer)
            // We drive zoom manually above on both platforms (walkers' own zoom
            // gate does not fire for a background Area), so turn its gesture off.
            .zoom_gesture(false)
            // Keep walkers off bare-scroll entirely: with no ctrl-zoom and no
            // touches it also stops scroll-panning, so our wheel handler is the
            // only thing acting on the wheel. We pan by primary-button drag.
            .zoom_with_ctrl(false)
            .panning(allow_pan)
            .drag_pan_buttons(if allow_pan {
                egui::DragPanButtons::PRIMARY
            } else {
                egui::DragPanButtons::empty()
            });
        child.add(map);

        if let Some(rot) = rotation {
            let pivot = clip.center();
            let end = ui.ctx().graphics_mut(|g| g.entry(layer_id).next_idx());
            ui.ctx().graphics_mut(|g| {
                let list = g.entry(layer_id);
                for i in start.0..end.0 {
                    list.mutate_shape(egui::layers::ShapeIdx(i), |cs| {
                        rotate_shape(&mut cs.shape, rot, pivot);
                        cs.clip_rect = clip;
                    });
                }
            });
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

impl MyApp {
    /// The interactive map page: full-bleed map with the floating controls.
    fn map_page(&mut self, ctx: &egui::Context, screen: egui::Rect) {
        // Box selection needs the screen to map 1:1 onto the north-up tile
        // space (the projector knows nothing about our post-rotation), so
        // heading-up rotation pauses while a region is being selected.
        let selecting = !matches!(self.select, RegionSelect::Inactive);

        // Rotate the map so the heading points up, when enabled and known. The
        // drawn angle eases toward the live heading each frame (shortest way
        // round the circle), so the map glides rather than stepping between
        // sensor updates. We keep requesting repaints until it settles.
        let rotation = match (self.heading_up && !selecting, self.effective_heading()) {
            (true, Some(target)) => {
                let dt = ctx.input(|i| i.stable_dt).clamp(0.0, 0.1);
                let current = self.smoothed_heading.unwrap_or(target);
                // Signed shortest angular distance to the target, in (-180, 180].
                let delta = (target - current + 540.0).rem_euclid(360.0) - 180.0;
                // Time-constant easing so the feel is frame-rate independent.
                let alpha = 1.0 - (-dt / 0.12).exp();
                let next = (current + delta * alpha).rem_euclid(360.0);
                self.smoothed_heading = Some(next);
                if delta.abs() > 0.05 {
                    ctx.request_repaint();
                }
                Some(Rot2::from_angle(-next.to_radians()))
            }
            _ => {
                self.smoothed_heading = None;
                None
            }
        };

        // Heading-up locks the map to the current position: it stays centered on
        // you (re-following each frame), which also makes dragging a no-op so the
        // rotated view can't be panned off. Zoom (buttons) still works.
        if self.heading_up && self.current.is_some() {
            self.map_memory.follow_my_position();
        }

        // A rotated map needs to paint past the screen edges, otherwise the
        // corners rotate away to nothing. Overscan to a square whose side is the
        // screen diagonal - large enough to cover the screen at any angle.
        let map_rect = if rotation.is_some() {
            egui::Rect::from_center_size(screen.center(), egui::Vec2::splat(screen.size().length()))
        } else {
            screen
        };

        // Full-bleed map in the background layer. It lives in its own Area (not a
        // CentralPanel) so its clip rect can extend past the screen for overscan.
        egui::Area::new(egui::Id::new("map"))
            .order(egui::Order::Background)
            .fixed_pos(map_rect.min)
            .movable(false)
            .constrain(false)
            .show(ctx, |ui| {
                ui.set_clip_rect(map_rect);
                self.map(ui, map_rect, rotation, screen);
            });

        // Double-click/tap a marker to show its name and time since last update.
        // Skipped while a region box is being drawn (double-clicks belong to it).
        if !selecting {
            self.marker_info(ctx, screen, map_rect, rotation);
        }

        // The box-selection layer sits between the map and the controls.
        self.select_overlay(ctx, screen);

        // Controls float on top in the foreground layer, so they keep pointer
        // priority over the (interactive) map behind them. The fill spans the
        // status-bar area; the top inset pushes the buttons clear of it.
        let top = self.top_inset(ctx);
        egui::Area::new(egui::Id::new("controls"))
            .order(egui::Order::Foreground)
            .fixed_pos(egui::Pos2::ZERO)
            .movable(false)
            .constrain(false)
            .show(ctx, |ui| {
                egui::Frame::NONE
                    .fill(ui.visuals().panel_fill)
                    .inner_margin(egui::Margin::symmetric(8, 4))
                    .show(ui, |ui| {
                        ui.set_width(screen.width());
                        ui.add_space(top);
                        self.controls(ui, screen);
                    });
            });

        // Selection hint / download confirmation, floating over everything.
        self.select_ui(ctx, screen);
    }

    /// Handle double-click/tap selection of a map marker and draw the info
    /// popup (name + time since last update) for the selected one.
    ///
    /// Marker screen positions are computed the same way the [`GpsLayer`] plugin
    /// draws them: project with the map's projector, then apply the heading-up
    /// rotation (about the screen center) when the map is rotated. A double-click
    /// that misses every marker dismisses the popup.
    fn marker_info(
        &mut self,
        ctx: &egui::Context,
        screen: egui::Rect,
        map_rect: egui::Rect,
        rotation: Option<Rot2>,
    ) {
        let my_position = self.current.unwrap_or_else(default_position);
        let projector = Projector::new(map_rect, &self.map_memory, my_position);
        let origin = screen.center();
        let to_screen = |pos: Position| {
            let p = projector.project(pos).to_pos2();
            match rotation {
                Some(rot) => rotate_pos(p, rot, origin),
                None => p,
            }
        };

        // Present markers, nearest-first is resolved below by distance.
        let markers: [(MarkerKind, Option<Position>); 2] =
            [(MarkerKind::You, self.current), (MarkerKind::Beacon, self.beacon)];

        // On a double-click, pick the closest marker within the hit radius; a
        // miss clears the current selection.
        let double = ctx.input(|i| i.pointer.button_double_clicked(egui::PointerButton::Primary));
        if double {
            if let Some(click) = ctx.input(|i| i.pointer.interact_pos()) {
                self.selected_marker = markers
                    .iter()
                    .filter_map(|(kind, pos)| {
                        pos.as_ref().map(|p| (*kind, to_screen(*p).distance(click)))
                    })
                    .filter(|(_, dist)| *dist <= MARKER_HIT_RADIUS)
                    .min_by(|a, b| a.1.total_cmp(&b.1))
                    .map(|(kind, _)| kind);
            }
        }

        let Some(kind) = self.selected_marker else { return };
        let (pos, time) = match kind {
            MarkerKind::You => (self.current, self.current_time),
            MarkerKind::Beacon => (self.beacon, self.beacon_time),
        };
        // The marker may have vanished (e.g. beacon disconnected) since it was
        // selected; drop the popup if so.
        let Some(pos) = pos else {
            self.selected_marker = None;
            return;
        };
        let anchor = to_screen(pos);

        let now = SystemTime::now();
        let age = match time {
            Some(t) => format!("Updated {} ago", age_text(now, t)),
            None => "No update yet".to_string(),
        };

        egui::Area::new(egui::Id::new("marker_info"))
            .order(egui::Order::Foreground)
            .fixed_pos(egui::pos2(anchor.x, anchor.y - 14.0))
            .pivot(egui::Align2::CENTER_BOTTOM)
            .movable(false)
            .constrain(true)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.label(egui::RichText::new(kind.label()).strong());
                    ui.label(age);
                });
            });
        // Keep the elapsed-time text live even without new fixes.
        ctx.request_repaint_after(std::time::Duration::from_secs(1));
    }

    /// The box-drag layer for the offline region download. It sits between the
    /// map (Background) and the floating controls (Foreground): drags land here
    /// instead of panning the map, while the buttons above stay clickable.
    fn select_overlay(&mut self, ctx: &egui::Context, screen: egui::Rect) {
        if matches!(self.select, RegionSelect::Inactive) {
            return;
        }
        let my_position = self.current.unwrap_or_else(default_position);
        let color = self.config.colors.track;
        let fill = color.gamma_multiply(0.15);
        let stroke = egui::Stroke::new(2.0, color);

        egui::Area::new(egui::Id::new("region_select"))
            .order(egui::Order::Middle)
            .fixed_pos(egui::Pos2::ZERO)
            .movable(false)
            .constrain(false)
            .show(ctx, |ui| match self.select {
                RegionSelect::Inactive => {}
                RegionSelect::Picking { mut start, mut current } => {
                    let resp = ui.allocate_rect(screen, egui::Sense::drag());
                    if resp.drag_started() {
                        start = resp.interact_pointer_pos();
                    }
                    if let Some(p) = resp.interact_pointer_pos() {
                        current = Some(p);
                    }
                    if let (Some(s), Some(c)) = (start, current) {
                        ui.painter().rect(
                            egui::Rect::from_two_pos(s, c),
                            egui::CornerRadius::ZERO,
                            fill,
                            stroke,
                            egui::StrokeKind::Middle,
                        );
                    }

                    self.select = match (resp.drag_stopped(), start, current) {
                        (true, Some(s), Some(c)) => {
                            let rect = egui::Rect::from_two_pos(s, c);
                            // Ignore taps and hairline drags.
                            if rect.width() >= 10.0 && rect.height() >= 10.0 {
                                // Same clip rect and position the map was
                                // drawn with (selection forces north-up, so
                                // the map rect is exactly the screen).
                                let projector =
                                    Projector::new(screen, &self.map_memory, my_position);
                                // Offer two zoom levels past the current view.
                                let max_zoom = (self.map_memory.zoom().ceil() as u8)
                                    .saturating_add(2)
                                    .min(17);
                                RegionSelect::Confirm {
                                    a: projector.unproject(rect.min.to_vec2()),
                                    b: projector.unproject(rect.max.to_vec2()),
                                    max_zoom,
                                }
                            } else {
                                RegionSelect::Picking { start: None, current: None }
                            }
                        }
                        (true, ..) => RegionSelect::Picking { start: None, current: None },
                        (false, ..) => RegionSelect::Picking { start, current },
                    };
                }
                RegionSelect::Confirm { a, b, .. } => {
                    let projector = Projector::new(screen, &self.map_memory, my_position);
                    let rect = egui::Rect::from_two_pos(
                        projector.project(a).to_pos2(),
                        projector.project(b).to_pos2(),
                    );
                    ui.painter().rect(
                        rect,
                        egui::CornerRadius::ZERO,
                        fill,
                        stroke,
                        egui::StrokeKind::Middle,
                    );
                }
            });
    }

    /// The floating hint while picking a box, and the confirm panel (tile
    /// count, max-zoom stepper) once one is chosen.
    fn select_ui(&mut self, ctx: &egui::Context, screen: egui::Rect) {
        let top = self.top_inset(ctx);
        match self.select {
            RegionSelect::Inactive => {}
            RegionSelect::Picking { .. } => {
                egui::Area::new(egui::Id::new("select_hint"))
                    .order(egui::Order::Foreground)
                    .fixed_pos(egui::Pos2::new(screen.center().x, top + 64.0))
                    .pivot(egui::Align2::CENTER_TOP)
                    .movable(false)
                    .constrain(false)
                    .show(ctx, |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                            ui.label("Drag a box over the region to download");
                        });
                    });
            }
            RegionSelect::Confirm { a, b, mut max_zoom } => {
                let mut close = false;
                egui::Area::new(egui::Id::new("select_confirm"))
                    .order(egui::Order::Foreground)
                    .fixed_pos(screen.center())
                    .pivot(egui::Align2::CENTER_CENTER)
                    .movable(false)
                    .constrain(false)
                    .show(ctx, |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                            ui.spacing_mut().button_padding = egui::vec2(15.0, 10.0);
                            ui.label(
                                egui::RichText::new("Download region for offline use").strong(),
                            );
                            ui.add_space(8.0);
                            ui.horizontal(|ui| {
                                ui.label("Max zoom:");
                                if ui.add_enabled(max_zoom > 1, egui::Button::new("-")).clicked() {
                                    max_zoom -= 1;
                                }
                                ui.label(format!("{max_zoom}"));
                                if ui.add_enabled(max_zoom < 19, egui::Button::new("+")).clicked()
                                {
                                    max_zoom += 1;
                                }
                            });
                            let count = offline::tile_count(a, b, max_zoom);
                            ui.label(format!(
                                "{count} tiles, ~{} MB",
                                (count * TILE_SIZE_ESTIMATE_KB).div_ceil(1024).max(1)
                            ));
                            if count > MAX_REGION_TILES {
                                ui.colored_label(
                                    egui::Color32::from_rgb(220, 80, 60),
                                    "Too many tiles: shrink the box or lower the max zoom.",
                                );
                            }
                            ui.add_space(8.0);
                            ui.horizontal(|ui| {
                                let can_download =
                                    count <= MAX_REGION_TILES && self.cache_dir.is_some();
                                if ui
                                    .add_enabled(can_download, egui::Button::new("Download"))
                                    .clicked()
                                {
                                    if let Some(dir) = &self.cache_dir {
                                        self.download = Some(offline::spawn_download(
                                            dir.clone(),
                                            offline::region_tiles(a, b, max_zoom),
                                            ctx.clone(),
                                        ));
                                    }
                                    close = true;
                                }
                                if ui.button("Cancel").clicked() {
                                    close = true;
                                }
                            });
                        });
                    });
                self.select = if close {
                    RegionSelect::Inactive
                } else {
                    RegionSelect::Confirm { a, b, max_zoom }
                };
            }
        }
    }

    /// Progress readout for the offline tile download, floating bottom-left
    /// on every page.
    fn download_ui(&mut self, ctx: &egui::Context, screen: egui::Rect) {
        let Some(progress) = self.download.clone() else { return };
        let bottom = self.bottom_inset(ctx);
        egui::Area::new(egui::Id::new("download_progress"))
            .order(egui::Order::Foreground)
            .fixed_pos(screen.left_bottom() + egui::vec2(8.0, -(8.0 + bottom)))
            .pivot(egui::Align2::LEFT_BOTTOM)
            .movable(false)
            .constrain(false)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    let done = progress.done.load(Ordering::Relaxed);
                    let failed = progress.failed.load(Ordering::Relaxed);
                    let status = if progress.finished() {
                        if failed > 0 {
                            format!("Offline tiles: done, {failed} of {} failed", progress.total)
                        } else {
                            format!("Offline tiles: all {} done", progress.total)
                        }
                    } else if failed > 0 {
                        format!("Offline tiles: {done}/{} ({failed} failed)", progress.total)
                    } else {
                        format!("Offline tiles: {done}/{}", progress.total)
                    };
                    ui.horizontal(|ui| {
                        ui.label(status);
                        let button = if progress.finished() { "OK" } else { "Cancel" };
                        if ui.button(button).clicked() {
                            progress.cancel.store(true, Ordering::Relaxed);
                            self.download = None;
                        }
                    });
                });
            });
    }

    /// The points page: a searchable, filterable list of every recorded GPS
    /// point from both sources. Tapping a row shows it on the map.
    fn points_page(&mut self, ctx: &egui::Context, screen: egui::Rect) {
        let top = self.top_inset(ctx);
        let bottom = self.bottom_inset(ctx);
        egui::Area::new(egui::Id::new("points"))
            .order(egui::Order::Background)
            .fixed_pos(egui::Pos2::ZERO)
            .movable(false)
            .constrain(false)
            .show(ctx, |ui| {
                egui::Frame::NONE
                    .fill(ui.visuals().panel_fill)
                    .inner_margin(egui::Margin::same(16))
                    .show(ui, |ui| {
                        ui.set_min_size(screen.size());
                        ui.add_space(top + 8.0);
                        ui.heading("GPS points");
                        ui.add_space(12.0);

                        ui.horizontal(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut self.points_search)
                                    .hint_text("search (e.g. 51.47 or esp)")
                                    .desired_width((screen.width() - 140.0).clamp(120.0, 320.0)),
                            );
                            if ui.button("Clear").clicked() {
                                self.points_search.clear();
                            }
                        });
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            ui.label("Source:");
                            ui.selectable_value(&mut self.points_filter, PointFilter::All, "all");
                            ui.selectable_value(
                                &mut self.points_filter,
                                PointFilter::Phone,
                                "phone",
                            );
                            ui.selectable_value(&mut self.points_filter, PointFilter::Esp, "esp");
                        });
                        ui.add_space(8.0);

                        let query = self.points_search.trim().to_lowercase();
                        let mut rows: Vec<TrackPoint> = self
                            .track
                            .iter()
                            .chain(self.beacon_track.iter())
                            .filter(|p| self.points_filter.admits(p.source))
                            .filter(|p| query.is_empty() || p.matches(&query))
                            .copied()
                            .collect();
                        // Newest first; the two tracks interleave by record time.
                        rows.sort_by(|x, y| y.time.cmp(&x.time));

                        let total = self.track.len() + self.beacon_track.len();
                        ui.label(format!("{} of {total} points", rows.len()));
                        ui.add_space(4.0);

                        let now = SystemTime::now();
                        let row_height = ui.text_style_height(&egui::TextStyle::Monospace);
                        let list_height =
                            (screen.bottom() - bottom - ui.cursor().min.y - 16.0).max(60.0);
                        let mut goto: Option<Position> = None;
                        egui::ScrollArea::vertical()
                            .max_height(list_height)
                            .auto_shrink([false, false])
                            .show_rows(ui, row_height, rows.len(), |ui, range| {
                                for p in &rows[range] {
                                    let text = format!(
                                        "{:<6} {}  {:>7}",
                                        p.source.label(),
                                        p.coord_text(),
                                        age_text(now, p.time),
                                    );
                                    if ui
                                        .selectable_label(
                                            false,
                                            egui::RichText::new(text).monospace(),
                                        )
                                        .clicked()
                                    {
                                        goto = Some(p.pos);
                                    }
                                }
                            });
                        if let Some(pos) = goto {
                            self.map_memory.center_at(pos);
                            self.page = Page::Map;
                        }
                    });
            });
    }

    /// The data page: a plain, large read-out of the current latitude and
    /// longitude plus the beacon distance, centered on an otherwise empty
    /// screen.
    fn data_page(&mut self, ctx: &egui::Context, screen: egui::Rect) {
        egui::Area::new(egui::Id::new("data"))
            .order(egui::Order::Background)
            .fixed_pos(egui::Pos2::ZERO)
            .movable(false)
            .constrain(false)
            .show(ctx, |ui| {
                egui::Frame::NONE
                    .fill(ui.visuals().panel_fill)
                    .show(ui, |ui| {
                        ui.set_min_size(screen.size());
                        ui.vertical_centered(|ui| {
                            ui.add_space(screen.height() * 0.3);
                            match self.current {
                                Some(pos) => {
                                    ui.label(
                                        egui::RichText::new(format!("lat {:.5}", pos.y()))
                                            .size(40.0),
                                    );
                                    ui.add_space(8.0);
                                    ui.label(
                                        egui::RichText::new(format!("lon {:.5}", pos.x()))
                                            .size(40.0),
                                    );
                                    if let Some(m) = self.distance_to_beacon() {
                                        ui.add_space(24.0);
                                        ui.label(
                                            egui::RichText::new(format!(
                                                "dist {}",
                                                format_distance(m)
                                            ))
                                            .size(40.0),
                                        );
                                    }
                                }
                                None => {
                                    ui.label(
                                        egui::RichText::new("waiting for GPS fix...").size(24.0),
                                    );
                                }
                            }

                            // The beacon's own data, when it is streaming.
                            if let (Some(b), Some(p)) = (self.beacon, self.beacon_packet) {
                                ui.add_space(24.0);
                                ui.label(
                                    egui::RichText::new(format!(
                                        "beacon {:.5} {:.5}",
                                        b.y(),
                                        b.x()
                                    ))
                                    .size(24.0),
                                );
                                ui.label(
                                    egui::RichText::new(format!(
                                        "sats {}  speed {:.1} m/s",
                                        p.sats,
                                        p.speed_mps()
                                    ))
                                    .size(24.0),
                                );
                            }
                        });
                    });
            });
    }

    /// Bottom-anchored bar for entering a position by hand when no live GPS
    /// source is wired up (desktop). Accepts "lat, lon" or "lat lon"; a valid
    /// entry feeds the same pipeline a real fix would and recenters the map.
    fn manual_gps_bar(&mut self, ctx: &egui::Context, screen: egui::Rect) {
        let bottom = self.bottom_inset(ctx);
        let margin = screen.size().min_elem() * CORNER_MARGIN_FRAC;
        egui::Area::new(egui::Id::new("manual_gps"))
            .order(egui::Order::Foreground)
            .fixed_pos(egui::Pos2::new(
                screen.center().x,
                screen.bottom() - bottom - margin,
            ))
            .pivot(egui::Align2::CENTER_BOTTOM)
            .movable(false)
            .constrain(false)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Position:");
                        let field = egui::TextEdit::singleline(&mut self.manual_gps_text)
                            .hint_text("lat, lon")
                            .desired_width((screen.width() * 0.5).clamp(140.0, 320.0));
                        let resp = ui.add(field);
                        let entered =
                            resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        if ui.button("Set").clicked() || entered {
                            match parse_lat_lon(&self.manual_gps_text) {
                                Some((lat, lon)) => {
                                    self.manual_gps_bad = false;
                                    self.apply_gps_fix(GpsFix {
                                        lat,
                                        lon,
                                        bearing: None,
                                    });
                                    self.map_memory.follow_my_position();
                                }
                                None => self.manual_gps_bad = true,
                            }
                        }
                    });
                    if self.manual_gps_bad {
                        ui.colored_label(
                            egui::Color32::from_rgb(220, 80, 60),
                            "Enter latitude and longitude, e.g. 51.4779, -0.0015",
                        );
                    }
                });
            });
    }

    /// The status page: board health for the esp32c6-gps board. The BLE link
    /// state comes from the connection itself; the WIO/GPS/LoRa figures come
    /// from the board's telemetry characteristic, and the last line from its
    /// log characteristic.
    fn status_page(&mut self, ctx: &egui::Context, screen: egui::Rect) {
        let top = self.top_inset(ctx);
        egui::Area::new(egui::Id::new("status"))
            .order(egui::Order::Background)
            .fixed_pos(egui::Pos2::ZERO)
            .movable(false)
            .constrain(false)
            .show(ctx, |ui| {
                egui::Frame::NONE
                    .fill(ui.visuals().panel_fill)
                    .inner_margin(egui::Margin::same(16))
                    .show(ui, |ui| {
                        ui.set_min_size(screen.size());
                        ui.add_space(top + 8.0);
                        egui::ScrollArea::vertical().show(ui, |ui| {
                            ui.heading("Status");
                            ui.add_space(12.0);

                            // ESP32-C6 / BLE link.
                            ui.strong("ESP32-C6 (BLE)");
                            status_bool(ui, "Link", self.ble_connected);
                            ui.label(self.ble_status.as_str());

                            let Some(t) = self.telemetry else {
                                ui.add_space(16.0);
                                ui.label(
                                    "No board telemetry yet.\n\
                                     Waiting for the esp32c6-gps board (an esp32c3 \
                                     beacon does not report it).",
                                );
                                if let Some(line) = &self.board_log {
                                    ui.add_space(16.0);
                                    ui.strong("Last message");
                                    ui.label(egui::RichText::new(line).monospace());
                                }
                                return;
                            };

                            // GPS (via the WIO's MAX-M10).
                            ui.add_space(16.0);
                            ui.strong("GPS");
                            status_bool(ui, "Fix", t.flags & TELEM_FLAG_GPS_FIX != 0);
                            ui.label(format!("Satellites: {}", t.sats));

                            // LoRa mesh link (WIO-E5 radio).
                            ui.add_space(16.0);
                            ui.strong("LoRa");
                            let last_rx = match t.secs_since_rx {
                                0xFFFF => "never".to_string(),
                                s => format!("{s} s ago"),
                            };
                            ui.label(format!("Last RX: {last_rx}"));
                            if t.last_rssi != 0 {
                                ui.label(format!(
                                    "RSSI: {} dBm   SNR: {:.2} dB",
                                    t.last_rssi,
                                    t.last_snr_cb as f32 / 100.0
                                ));
                            }
                            ui.label(format!("RX: {}   TX: {}", t.rx_count, t.tx_count));

                            // WIO-E5 housekeeping.
                            ui.add_space(16.0);
                            ui.strong("WIO-E5");
                            status_bool(ui, "SD logging", t.flags & TELEM_FLAG_SD_OK != 0);
                            status_bool(ui, "Radio config", t.flags & TELEM_FLAG_CFG_LOADED != 0);

                            if let Some(line) = &self.board_log {
                                ui.add_space(16.0);
                                ui.strong("Last message");
                                ui.label(egui::RichText::new(line).monospace());
                            }
                        });
                    });
            });
    }

    /// The settings page: load a TOML config file, and talk to the BLE
    /// beacon (status, notify interval with device ack, path toggle).
    fn settings_page(&mut self, ctx: &egui::Context, screen: egui::Rect) {
        let top = self.top_inset(ctx);
        egui::Area::new(egui::Id::new("settings"))
            .order(egui::Order::Background)
            .fixed_pos(egui::Pos2::ZERO)
            .movable(false)
            .constrain(false)
            .show(ctx, |ui| {
                egui::Frame::NONE
                    .fill(ui.visuals().panel_fill)
                    .inner_margin(egui::Margin::same(16))
                    .show(ui, |ui| {
                        ui.set_min_size(screen.size());
                        ui.add_space(top + 8.0);
                        ui.heading("Settings");
                        ui.add_space(12.0);

                        ui.label("Config file (TOML):");
                        ui.horizontal(|ui| {
                            let field = egui::TextEdit::singleline(&mut self.config_path)
                                .hint_text("/path/to/config.toml")
                                .desired_width((screen.width() - 120.0).clamp(120.0, 400.0));
                            let resp = ui.add(field);
                            let entered =
                                resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                            if ui.button("Load").clicked() || entered {
                                self.load_config();
                            }
                        });

                        ui.add_space(8.0);
                        match &self.config_feedback {
                            Some(Ok(msg)) => {
                                ui.colored_label(egui::Color32::from_rgb(60, 180, 75), msg);
                            }
                            Some(Err(msg)) => {
                                ui.colored_label(egui::Color32::from_rgb(220, 80, 60), msg);
                            }
                            None => {}
                        }

                        ui.add_space(16.0);
                        ui.label("Marker colors:");
                        color_swatch(ui, "track", self.config.colors.track);
                        color_swatch(ui, "beacon", self.config.colors.fixed);

                        ui.add_space(16.0);
                        ui.separator();
                        ui.add_space(8.0);
                        ui.heading("GPS beacon (BLE)");
                        ui.add_space(8.0);
                        ui.label(format!("Status: {}", self.ble_status));
                        ui.add_space(8.0);
                        ui.checkbox(&mut self.show_beacon_path, "Show beacon path on map");

                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            ui.label("Notify interval (ms):");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.ble_interval_text)
                                    .desired_width(80.0),
                            );
                            let apply =
                                ui.add_enabled(self.ble_connected, egui::Button::new("Apply"));
                            if apply.clicked() {
                                match self.ble_interval_text.trim().parse::<u32>() {
                                    Ok(ms) => {
                                        let _ =
                                            self.ble.commands.send(BleCommand::SetInterval(ms));
                                        self.ble_ack = None;
                                        self.ble_ack_pending = true;
                                    }
                                    Err(_) => {
                                        self.ble_ack = Some(Err(
                                            "Enter a whole number of milliseconds.".to_string()
                                        ));
                                    }
                                }
                            }
                        });
                        match &self.ble_ack {
                            Some(Ok(msg)) => {
                                ui.colored_label(egui::Color32::from_rgb(60, 180, 75), msg);
                            }
                            Some(Err(msg)) => {
                                ui.colored_label(egui::Color32::from_rgb(220, 80, 60), msg);
                            }
                            None if self.ble_ack_pending => {
                                ui.label("waiting for device ack...");
                            }
                            None => {}
                        }

                        ui.add_space(8.0);
                        if ui.button("Reconnect").clicked() {
                            self.sync_ble_to_config();
                        }

                        ui.add_space(16.0);
                        ui.label(
                            egui::RichText::new(
                                "[colors]\ntrack = \"#0078ff\"\nfixed = \"#ff5028\"\n\n[ble]\nenabled = true\nshow_path = true\n# mac = \"AA:BB:CC:DD:EE:FF\"\n\n[track]\nmin_distance = 3.0",
                            )
                            .monospace()
                            .size(13.0),
                        );
                    });
            });
    }

    /// Dropdown menu to jump straight to any page. Replaces the old next-page
    /// cycler; the current page is marked. Rendered inline in the map controls
    /// bar and in the floating corner toggle on other pages. The trigger glyph
    /// crossfades from the hamburger to an X while the menu is open.
    fn page_menu(&mut self, ui: &mut egui::Ui, icon: f32) {
        let text = ui.visuals().text_color();
        // Transparent base image: it reserves the icon-sized hit area and owns
        // the click/menu behavior; the visible glyph is painted on top so it
        // can crossfade between the hamburger and the X.
        let base = egui::Image::new(egui::include_image!("../assets/icons/menu.svg"))
            .fit_to_exact_size(egui::vec2(icon, icon))
            .tint(egui::Color32::TRANSPARENT);
        let resp = ui.menu_image_button(base, |ui| {
            for (page, label, src) in page_items() {
                let item_icon = egui::Image::new(src)
                    .fit_to_exact_size(egui::vec2(16.0, 16.0))
                    .tint(ui.visuals().text_color());
                let selected = self.page == page;
                if ui
                    .add(egui::Button::image_and_text(item_icon, label).selected(selected))
                    .clicked()
                {
                    self.page = page;
                    ui.close();
                }
            }
        });

        // `inner` is `Some` only while the menu popup is shown, so it drives the
        // open/close crossfade. `animate_bool_with_time` eases it and keeps
        // requesting repaints until it settles.
        let open = resp.inner.is_some();
        let rect =
            egui::Rect::from_center_size(resp.response.rect.center(), egui::vec2(icon, icon));
        let t = ui
            .ctx()
            .animate_bool_with_time(egui::Id::new("page_menu_icon_anim"), open, 0.15);
        egui::Image::new(egui::include_image!("../assets/icons/menu.svg"))
            .tint(text.gamma_multiply(1.0 - t))
            .paint_at(ui, rect);
        egui::Image::new(egui::include_image!("../assets/icons/close.svg"))
            .tint(text.gamma_multiply(t))
            .paint_at(ui, rect);

        resp.response.on_hover_text("Pages");
    }

    /// Floating page menu in the top-right corner. Used on every page but the
    /// map, where the menu lives at the right end of the controls bar instead.
    fn page_toggle(&mut self, ctx: &egui::Context, screen: egui::Rect) {
        let size = icon_size_for(screen);
        let top = self.top_inset(ctx);
        // Corner inset as a fraction of the screen, so the button stays clear
        // of the edge on any size (a fixed few points crowds a dense screen).
        let margin = screen.size().min_elem() * CORNER_MARGIN_FRAC;
        egui::Area::new(egui::Id::new("page_toggle"))
            // Float above the (Background) page content it sits over.
            .order(egui::Order::Tooltip)
            .fixed_pos(egui::Pos2::new(screen.right() - margin, top + margin))
            .pivot(egui::Align2::RIGHT_TOP)
            .movable(false)
            .constrain(false)
            .show(ctx, |ui| {
                ui.spacing_mut().button_padding = egui::vec2(size * 0.7, size * 0.45);
                self.page_menu(ui, size);
            });
    }
}

/// Parse "lat, lon" or "lat lon" into decimal degrees. `None` unless it is
/// exactly two finite numbers within the valid latitude/longitude range.
fn parse_lat_lon(s: &str) -> Option<(f64, f64)> {
    let mut parts = s
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|p| !p.is_empty());
    let lat: f64 = parts.next()?.parse().ok()?;
    let lon: f64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None; // trailing junk
    }
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return None;
    }
    Some((lat, lon))
}

/// A labeled boolean status row: the label followed by a green "yes" or a red
/// "no", for the Status page's health indicators.
fn status_bool(ui: &mut egui::Ui, label: &str, ok: bool) {
    ui.horizontal(|ui| {
        ui.label(format!("{label}:"));
        let (text, color) = if ok {
            ("yes", egui::Color32::from_rgb(60, 180, 75))
        } else {
            ("no", egui::Color32::from_rgb(220, 80, 60))
        };
        ui.colored_label(color, text);
    });
}

/// A small filled square in `color` followed by its name and hex value.
fn color_swatch(ui: &mut egui::Ui, label: &str, color: egui::Color32) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::Vec2::splat(18.0), egui::Sense::hover());
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::same(3), color);
        ui.label(format!(
            "{label}  #{:02x}{:02x}{:02x}",
            color.r(),
            color.g(),
            color.b()
        ));
    });
}

/// Rotate `p` by `rot` about `origin` (screen-space points).
fn rotate_pos(p: Pos2, rot: Rot2, origin: Pos2) -> Pos2 {
    origin + rot * (p - origin)
}

/// Rotate a painted [`Shape`] in place about `origin`. Mirrors the point-moving
/// arm of `Shape::transform`, but applies a rotation instead of a scale/offset.
/// Axis-aligned rects and callbacks can only follow their center; everything
/// else (meshes, paths, text) rotates faithfully.
fn rotate_shape(shape: &mut Shape, rot: Rot2, origin: Pos2) {
    match shape {
        Shape::Noop => {}
        Shape::Vec(shapes) => {
            for s in shapes {
                rotate_shape(s, rot, origin);
            }
        }
        Shape::Circle(c) => c.center = rotate_pos(c.center, rot, origin),
        Shape::Ellipse(e) => e.center = rotate_pos(e.center, rot, origin),
        Shape::LineSegment { points, .. } => {
            for p in points {
                *p = rotate_pos(*p, rot, origin);
            }
        }
        Shape::Path(path) => {
            for p in &mut path.points {
                *p = rotate_pos(*p, rot, origin);
            }
        }
        Shape::Rect(r) => {
            let center = rotate_pos(r.rect.center(), rot, origin);
            r.rect = egui::Rect::from_center_size(center, r.rect.size());
        }
        Shape::Text(t) => {
            t.pos = rotate_pos(t.pos, rot, origin);
            t.angle += rot.angle();
        }
        Shape::Mesh(mesh) => std::sync::Arc::make_mut(mesh).rotate(rot, origin),
        Shape::QuadraticBezier(b) => {
            for p in &mut b.points {
                *p = rotate_pos(*p, rot, origin);
            }
        }
        Shape::CubicBezier(b) => {
            for p in &mut b.points {
                *p = rotate_pos(*p, rot, origin);
            }
        }
        Shape::Callback(cb) => {
            let center = rotate_pos(cb.rect.center(), rot, origin);
            cb.rect = egui::Rect::from_center_size(center, cb.rect.size());
        }
    }
}
