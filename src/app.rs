use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use egui::Pos2;
use gps_proto::packet::{self, Ack, PositionPacket};
use midair_proto::ble;
use walkers::{
    lat_lon, sources::OpenStreetMap, HeaderValue, HttpOptions, HttpTiles, MapMemory, Position,
    Projector,
};

use crate::ble::{BleCommand, BleEvent, BleHandle, ConfigWrite, Settings, Telemetry};
use crate::compass::CompassHandle;
use crate::config::AppConfig;
use crate::gps::GpsFix;
use crate::offline::{self, DownloadProgress};
use crate::points::{PointSource, TrackPoint};
use crate::radio::{EditVal, RadioDoc};
use crate::tiles::{MapLayer, OpenTopoMap};

/// The view layer (page rendering + shared egui scaffolding). Kept in a
/// submodule so this file holds only state and the core update logic; the
/// `impl MyApp` blocks there render each page.
mod ui;

/// The name of a config setting, for the ack line.
fn setting_name(id: u8) -> &'static str {
    match id {
        packet::CFG_UPDATE_INTERVAL_MS => "notify interval",
        ble::CFG_PWR_EN => "GPS/LoRa power",
        ble::CFG_WIO_SLEEP => "WIO-E5 sleep",
        ble::CFG_GPS_SLEEP => "GPS backup mode",
        ble::CFG_ESP_SLEEP_S => "wake-check interval",
        _ => "setting",
    }
}

/// What the board said about the last config write. On success this reports
/// the value it actually applied, which for the intervals may be a clamped
/// version of what was asked for. The on/off settings ack without a value;
/// their new state arrives in the settings blob instead.
fn ack_message(ack: &Ack) -> Result<String, String> {
    let name = setting_name(ack.id);
    let applied = ack.value_u32.unwrap_or(0);
    match ack.status {
        packet::ACK_OK => Ok(match ack.id {
            packet::CFG_UPDATE_INTERVAL_MS => {
                format!("Board applied: notify interval {applied} ms")
            }
            ble::CFG_ESP_SLEEP_S if applied == 0 => "Board applied: sleep disabled".to_string(),
            ble::CFG_ESP_SLEEP_S => {
                format!("Board applied: wake check every {}", secs_text(applied))
            }
            _ => format!("Board applied: {name}"),
        }),
        packet::ACK_UNKNOWN_ID => Err(format!(
            "Board rejected: it does not know the {name} setting"
        )),
        packet::ACK_BAD_VALUE => Err(format!("Board rejected: bad value for {name}")),
        // Not a rejected value: the ESP could not reach the WIO-E5 over the
        // UART link between them, so the setting never got there.
        ble::ACK_WIO_ERROR => Err(format!(
            "The board could not reach the WIO-E5 to set {name} (link error)."
        )),
        ble::ACK_WIO_TIMEOUT => Err(format!(
            "The board could not reach the WIO-E5 to set {name} (no reply)."
        )),
        ble::ACK_BAD_STATE => Err(format!(
            "Board rejected {name}: not valid in its current state"
        )),
        s => Err(format!("Board rejected {name}: status {s:#04x}")),
    }
}

/// What the user last told the BLE worker to do. Explicit rather than derived
/// from the config, because "leave the board alone" is a real thing to want:
/// the board only deep-sleeps while nothing is connected, so an app that
/// always reconnects keeps it awake and its sleep interval never does
/// anything. Disconnect is how you let it sleep.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum BleIntent {
    /// Stay off the air until asked. Nothing reconnects on its own.
    Idle,
    /// Connect, going straight to a pinned MAC when there is one.
    Connect,
    /// Connect expecting the board to be asleep: scan continuously so a wake
    /// window cannot be missed. Costs more radio time, so it is not the
    /// default.
    ConnectSleeping,
}

/// A duration in seconds as a short human phrase ("45 s", "5 min", "12 h").
/// Used wherever a sleep interval is shown or confirmed.
pub(crate) fn secs_text(s: u32) -> String {
    match s {
        0 => "off".to_string(),
        s if s < 60 => format!("{s} s"),
        s if s < 3600 => format!("{:.0} min", s as f32 / 60.0),
        s if s % 3600 == 0 => format!("{} h", s / 3600),
        s => format!("{:.1} h", s as f32 / 3600.0),
    }
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

/// Initial great-circle bearing from `a` to `b`, in degrees clockwise from
/// north. Tracking mode turns the map by this so the beacon points up.
fn bearing_deg(a: Position, b: Position) -> f32 {
    let lat1 = a.y().to_radians();
    let lat2 = b.y().to_radians();
    let dlon = (b.x() - a.x()).to_radians();
    let y = dlon.sin() * lat2.cos();
    let x = lat1.cos() * lat2.sin() - lat1.sin() * lat2.cos() * dlon.cos();
    y.atan2(x).to_degrees().rem_euclid(360.0) as f32
}

/// Config file loaded at startup and written back by the Settings page, unless
/// another path is typed there.
const DEFAULT_CONFIG_NAME: &str = "gps-config.toml";

/// Where the config file lives unless the Settings page is pointed elsewhere:
/// beside the tile cache, which on Android is the app's private data directory
/// (the working directory there is not writable, so a bare filename could be
/// read but never saved). On desktop the cache is a relative directory, which
/// leaves the plain filename in the working directory.
fn default_config_path(cache_dir: Option<&std::path::Path>) -> String {
    match cache_dir.and_then(std::path::Path::parent) {
        Some(dir) if !dir.as_os_str().is_empty() => {
            dir.join(DEFAULT_CONFIG_NAME).display().to_string()
        }
        _ => DEFAULT_CONFIG_NAME.to_string(),
    }
}

/// Tracking mode: fraction of the screen height kept clear above the beacon and
/// below the user, so neither marker sits hard against an edge.
const TRACK_MARGIN_FRAC: f32 = 0.18;
/// Zoom range the tracking auto-fit is clamped to.
const TRACK_ZOOM_MIN: f64 = 2.0;
const TRACK_ZOOM_MAX: f64 = 19.0;

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
    /// The BLE beacon: the link, and the board's own power and sleep settings.
    Beacon,
    /// The app's own settings, from the TOML config file (marker colors,
    /// overlay sizes, distance read-out, track recording, offline maps).
    Settings,
    /// Viewing and editing the WIO-E5 RADIO.TOML (radio, mesh, beacon, GPS).
    Radio,
}

/// Per-field edit flow on the Radio page. Only one field is in flight at a time:
/// the pencil opens the confirm popup, confirming unlocks the typed input, and
/// the check/x commit or discard.
#[derive(Clone, Default)]
pub enum RadioEdit {
    /// No field is being edited.
    #[default]
    None,
    /// The confirm popup is open for this field (Edit / Cancel).
    Confirm { section: String, key: String },
    /// The field is unlocked with a typed input plus a check and an x.
    Active {
        section: String,
        key: String,
        val: EditVal,
    },
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
    /// Standard OpenStreetMap tiles.
    tiles: HttpTiles,
    /// OpenTopoMap topographic tiles, shown when `layer` is `Topo`. Both share
    /// the same on-disk cache (keyed by URL) and the same `map_memory`.
    topo_tiles: HttpTiles,
    /// Which tile layer is currently drawn.
    layer: MapLayer,
    map_memory: MapMemory,
    /// Live GPS fixes, when a source is wired up (Android GNSS). `None` on
    /// desktop, where the manual position bar is shown instead.
    gps_rx: Option<Receiver<GpsFix>>,
    /// Device-facing compass, when the platform has one (Android only). The
    /// sensor behind it is powered only while heading-up needs it.
    compass: Option<CompassHandle>,
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
    /// Tracking mode: index into the available beacons of the one being kept in
    /// frame (user near the bottom, beacon near the top). `None` is off. The
    /// track button cycles it; the heading button exits.
    tracking_beacon: Option<usize>,
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
    /// Last BLE status line, for the Beacon page.
    ble_status: String,
    ble_connected: bool,
    /// Notify-interval input on the Beacon page.
    ble_interval_text: String,
    /// Result of the last config write: device ack (green) or error (red).
    ble_ack: Option<Result<String, String>>,
    /// A config write is in flight and the ack has not arrived yet.
    ble_ack_pending: bool,
    /// Latest board telemetry (esp32c6-gps), for the Status page.
    telemetry: Option<Telemetry>,
    /// Latest WIO status/log line relayed by the board.
    board_log: Option<String>,
    /// The board's own power and sleep settings, as it last reported them.
    /// The controls read this rather than any local copy: the board is the
    /// authority and changes these by itself (clamping an interval).
    board_settings: Option<Settings>,
    /// The board's settings layout is newer than this build can decode, so
    /// its settings are unknown rather than defaulted.
    settings_unsupported: bool,
    /// What the app is currently asking the BLE worker to do. Session state:
    /// `config.ble.enabled` seeds it at startup and nothing writes it back, so
    /// a Disconnect lasts until the next launch rather than becoming a setting.
    ble_intent: BleIntent,
    /// When `ble_intent` last changed, for the "trying for ..." read-out.
    intent_since: Instant,
    /// When the current connection came up. The GPS/LoRa rail only powers on
    /// once a central connects, so telemetry is legitimately empty for the
    /// first seconds and the Status page says warming up, not broken.
    connected_at: Option<Instant>,
    /// Wake-check interval input (seconds) on the Beacon page.
    sleep_interval_text: String,
    /// Advertising-window input (seconds) on the Beacon page.
    adv_window_text: String,
    /// Which screen is currently shown.
    page: Page,
    /// Loaded configuration (marker colors, BLE settings).
    config: AppConfig,
    /// The config-file path typed on the Settings page.
    config_path: String,
    /// Result of the last load/save: `Ok` message (green) or error (red).
    config_feedback: Option<Result<String, String>>,
    /// Text buffer behind the `[ble] mac` input. Empty means "scan by service",
    /// which is what `config.ble.mac == None` says.
    ble_mac_text: String,
    /// The WIO-E5 RADIO.TOML being edited on the Radio page, once loaded.
    radio: Option<RadioDoc>,
    /// The RADIO.TOML path typed on the Radio page.
    radio_path: String,
    /// Result of the last radio load/save: `Ok` message (green) or error (red).
    radio_feedback: Option<Result<String, String>>,
    /// Per-field edit flow on the Radio page.
    radio_edit: RadioEdit,
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
    /// The center button's marker list is open (held/right-clicked the button).
    /// A plain tap centers on you instead, without ever opening this.
    center_menu: bool,
    /// Offline center-button zoom fallback: the background probe sends the zoom
    /// level to snap to (nearest cached tile) here when it finds we are offline.
    zoom_tx: Sender<f64>,
    zoom_rx: Receiver<f64>,
    /// Width the map controls row took last frame, used to center it (egui
    /// can't center a horizontal row in a single layout pass). `0.0` until the
    /// first frame has measured it.
    controls_width: f32,
}

impl MyApp {
    /// `gps_rx` is the live GPS fix stream, or `None` when no source is wired
    /// up (desktop) - the UI then shows a manual position entry bar instead.
    /// `cache_dir` is where tiles are cached to disk (`None` to disable). Desktop
    /// passes a local `.cache`; Android passes its writable data directory.
    /// `compass` is the device-facing heading source (`None` on desktop).
    /// `insets` reports the safe-area insets in physical pixels (`None` on desktop).
    /// `ble` is the worker connected to the ESP32-C3 GPS beacon.
    pub fn new(
        ctx: egui::Context,
        gps_rx: Option<Receiver<GpsFix>>,
        cache_dir: Option<PathBuf>,
        compass: Option<CompassHandle>,
        insets: Option<Box<dyn Fn() -> [f32; 4]>>,
        ble: BleHandle,
    ) -> Self {
        // SVG loader for the button icons.
        egui_extras::install_image_loaders(&ctx);

        let (zoom_tx, zoom_rx) = std::sync::mpsc::channel();

        let mut app = Self {
            tiles: HttpTiles::with_options(
                OpenStreetMap,
                http_options(cache_dir.clone()),
                ctx.clone(),
            ),
            topo_tiles: HttpTiles::with_options(
                OpenTopoMap,
                http_options(cache_dir.clone()),
                ctx,
            ),
            layer: MapLayer::Standard,
            map_memory: MapMemory::default(),
            gps_rx,
            compass,
            ble,
            insets,
            current: None,
            current_time: None,
            heading: None,
            compass_heading: None,
            heading_up: false,
            tracking_beacon: None,
            smoothed_heading: None,
            track: Vec::new(),
            beacon: None,
            beacon_time: None,
            beacon_packet: None,
            beacon_track: Vec::new(),
            ble_status: "idle".to_string(),
            ble_connected: false,
            ble_interval_text: packet::UPDATE_INTERVAL_DEFAULT_MS.to_string(),
            ble_ack: None,
            ble_ack_pending: false,
            telemetry: None,
            board_log: None,
            board_settings: None,
            settings_unsupported: false,
            // Overwritten by `apply_config`/`sync_ble_to_config` below, which
            // is what actually decides whether to connect at startup.
            ble_intent: BleIntent::Idle,
            intent_since: Instant::now(),
            connected_at: None,
            // The low end of the clamp range, so a stray press arms the
            // shortest sleep rather than the longest.
            sleep_interval_text: ble::ESP_SLEEP_MIN_S.to_string(),
            // The firmware's own default, not the low end: a short window is
            // the hazardous direction here (it is what makes a sleeping board
            // hard to catch), so a stray press should ask for what an
            // unconfigured board already does.
            adv_window_text: ble::ESP_ADV_DEFAULT_S.to_string(),
            page: Page::Map,
            config: AppConfig::default(),
            // The path the auto-load below tries, so Save writes back to the
            // same file without the user having to type it.
            config_path: default_config_path(cache_dir.as_deref()),
            config_feedback: None,
            ble_mac_text: String::new(),
            radio: None,
            radio_path: "RADIO.toml".to_string(),
            radio_feedback: None,
            radio_edit: RadioEdit::None,
            cache_dir,
            select: RegionSelect::Inactive,
            download: None,
            points_search: String::new(),
            points_filter: PointFilter::All,
            manual_gps_text: String::new(),
            manual_gps_bad: false,
            selected_marker: None,
            center_menu: false,
            zoom_tx,
            zoom_rx,
            controls_width: 0.0,
        };

        // Auto-load the default config when present; the Settings page can load
        // any path later, and saves back to whichever one is in the box. With no
        // file the defaults apply, which include connecting to the beacon.
        let startup_path = app.config_path.clone();
        match AppConfig::load(&startup_path) {
            Ok(cfg) => app.apply_config(cfg),
            Err(_) => app.sync_ble_to_config(),
        }
        app
    }

    /// Adopt a loaded config: colors, the MAC input, and the BLE connection.
    fn apply_config(&mut self, cfg: AppConfig) {
        self.ble_mac_text = cfg.ble.mac.clone().unwrap_or_default();
        self.config = cfg;
        self.sync_ble_to_config();
    }

    /// Seed the intent from the config at startup: auto-connect unless the
    /// master switch is off.
    fn sync_ble_to_config(&mut self) {
        let intent = if self.config.ble.enabled {
            BleIntent::Connect
        } else {
            BleIntent::Idle
        };
        self.set_ble_intent(intent);
    }

    /// Ask the worker for a new connection state, and say so even when the
    /// intent has not changed - that is what makes the buttons re-send a
    /// request with an edited MAC, or restart a scan that has given up.
    ///
    /// Each button sends exactly one command. They must not be composed (a
    /// Disconnect followed by a Connect, say): the worker drains its whole
    /// queue in one pass, so the later command simply overwrites the earlier
    /// one and the disconnect never happens.
    pub(crate) fn set_ble_intent(&mut self, intent: BleIntent) {
        if self.ble_intent != intent {
            self.intent_since = Instant::now();
        }
        self.ble_intent = intent;
        let cmd = match intent {
            BleIntent::Idle => BleCommand::Disconnect,
            BleIntent::Connect => BleCommand::Connect {
                mac: self.config.ble.mac.clone(),
                chase: false,
            },
            BleIntent::ConnectSleeping => BleCommand::Connect {
                mac: self.config.ble.mac.clone(),
                chase: true,
            },
        };
        let _ = self.ble.commands.send(cmd);
    }

    /// What the app is doing about the link, for the Settings and Status
    /// pages. Separate from `ble_status`, which is the worker's own running
    /// commentary on the attempt.
    pub(crate) fn ble_intent_text(&self) -> String {
        let waiting = secs_text((self.intent_since.elapsed().as_secs() as u32).max(1));
        match (self.ble_intent, self.ble_connected) {
            (BleIntent::Idle, _) => "Not connecting. The board is free to sleep.".to_string(),
            (_, true) => "Connected. The board stays awake until you disconnect.".to_string(),
            (BleIntent::Connect, false) => format!("Connecting for {waiting}."),
            (BleIntent::ConnectSleeping, false) => {
                format!("Scanning for a sleeping board for {waiting}.")
            }
        }
    }

    /// Queue one config write to the board and wait for its ack. The controls
    /// stay disabled until the ack lands, so only one write is ever in flight
    /// and the state shown is always one the board has confirmed.
    pub(crate) fn send_config(&mut self, write: ConfigWrite) {
        let _ = self.ble.commands.send(BleCommand::Config(write));
        self.ble_ack = None;
        self.ble_ack_pending = true;
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

    /// Write the settings as they stand to `config_path`. An existing file is
    /// edited in place (comments and unknown keys survive); with no file there
    /// yet, a documented one is generated.
    fn save_config(&mut self) {
        let path = self.config_path.trim().to_string();
        if path.is_empty() {
            self.config_feedback = Some(Err("Enter a file path.".to_string()));
            return;
        }
        self.config_feedback = Some(match self.config.save(&path) {
            Ok(true) => Ok(format!("Created {path}")),
            Ok(false) => Ok(format!("Saved {path}")),
            Err(e) => Err(e),
        });
    }

    /// Drop every setting back to its built-in default. The file is untouched
    /// until the next save, so this is undoable by reloading.
    fn reset_config(&mut self) {
        self.apply_config(AppConfig::default());
        self.config_feedback = Some(Ok("Reset to defaults. Not saved yet.".to_string()));
    }

    /// Load the RADIO.TOML at `radio_path`, recording a human-readable result
    /// for the Radio page to show and clearing any in-flight edit.
    fn load_radio(&mut self) {
        let path = self.radio_path.trim().to_string();
        if path.is_empty() {
            self.radio_feedback = Some(Err("Enter a file path.".to_string()));
            return;
        }
        self.radio_edit = RadioEdit::None;
        self.radio_feedback = Some(match RadioDoc::load(&path) {
            Ok(doc) => {
                self.radio = Some(doc);
                Ok(format!("Loaded {path}"))
            }
            Err(e) => Err(e),
        });
    }

    /// Start a RADIO.TOML at the firmware defaults, aimed at `radio_path`.
    /// Nothing is written until Save, which backs up any existing file first,
    /// so this cannot lose a config by itself.
    fn default_radio(&mut self) {
        let path = self.radio_path.trim().to_string();
        if path.is_empty() {
            self.radio_feedback = Some(Err("Enter a file path.".to_string()));
            return;
        }
        self.radio_edit = RadioEdit::None;
        self.radio_feedback = Some(match RadioDoc::default_at(&path) {
            Ok(doc) => {
                self.radio = Some(doc);
                Ok(format!("Default config ready. Press Save to write {path}"))
            }
            Err(e) => Err(e),
        });
    }

    /// Write the edited RADIO.TOML back, backing up the previous file first.
    fn save_radio(&mut self) {
        let Some(doc) = self.radio.as_mut() else {
            return;
        };
        self.radio_feedback = Some(match doc.save() {
            Ok(Some(backup)) => {
                let name = backup
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default()
                    .to_string();
                Ok(format!("Saved. Backed up previous version as {name}"))
            }
            Ok(None) => Ok("Saved.".to_string()),
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

    /// Whether the map can be turned to a heading at all: either a heading is
    /// already known, or a compass exists that would supply one once powered.
    /// The heading-up button is shown on this rather than on a live reading,
    /// since the sensor is off until that button turns it on.
    fn has_direction(&self) -> bool {
        self.effective_heading().is_some() || self.compass.is_some()
    }

    /// Power the compass sensor for heading-up and nothing else.
    ///
    /// The rotation-vector sensor is fused from the accelerometer, gyroscope
    /// and magnetometer, so it keeps all three awake; heading-up is the only
    /// mode that draws a device heading. Tracking mode turns the map by the
    /// bearing to the beacon instead, and the marker's heading arrow falls back
    /// to course over ground.
    fn sync_compass_power(&mut self) {
        let Some(compass) = &self.compass else { return };
        let wanted = self.heading_up;
        // Switching off drops the last reading: it stops being updated, so
        // holding on to it would draw a heading that quietly goes stale.
        if compass.wanted.swap(wanted, Ordering::Relaxed) && !wanted {
            self.compass_heading = None;
        }
    }

    /// Center the map on `target`, leaving tracking mode (which recomputes the
    /// center every frame and would override this at once). `follow` re-follows
    /// the live position rather than pinning the map to one point, which is what
    /// centering on yourself should do.
    ///
    /// When tiles are cached to disk this also kicks off the offline check: if
    /// we turn out to be offline and the current zoom has no tile for `target`,
    /// the map snaps to the nearest zoom that does.
    fn center_on(&mut self, ctx: &egui::Context, target: Position, follow: bool) {
        self.tracking_beacon = None;
        if follow {
            self.map_memory.follow_my_position();
        } else {
            self.map_memory.center_at(target);
        }
        if let Some(dir) = self.cache_dir.clone() {
            let current_zoom = self.map_memory.zoom().round().clamp(0.0, 19.0) as u8;
            offline::spawn_offline_zoom(
                dir,
                self.layer,
                target,
                current_zoom,
                self.zoom_tx.clone(),
                ctx.clone(),
            );
        }
    }

    /// The markers the center button can center on, in menu order: you first
    /// (the plain tap's target), then each beacon. Only markers with a known
    /// position are listed, so an entry always has somewhere to go.
    fn center_targets(&self) -> Vec<(MarkerKind, Position)> {
        let mut targets: Vec<(MarkerKind, Position)> =
            self.current.map(|p| (MarkerKind::You, p)).into_iter().collect();
        targets.extend(
            self.beacon_positions()
                .into_iter()
                .map(|p| (MarkerKind::Beacon, p)),
        );
        targets
    }

    /// Great-circle distance from the current position to the BLE beacon, in
    /// meters. `None` until both a fix and a beacon position are known.
    fn distance_to_beacon(&self) -> Option<f64> {
        match (self.current, self.beacon) {
            (Some(cur), Some(beacon)) => Some(haversine_m(cur, beacon)),
            _ => None,
        }
    }

    /// The beacons that tracking mode can cycle through. One entry today (the
    /// single BLE beacon); the list keeps the cycling logic ready for more.
    fn beacon_positions(&self) -> Vec<Position> {
        self.beacon.into_iter().collect()
    }

    /// While tracking a beacon, recenter the map between the user and that
    /// beacon and pick a zoom that keeps both on screen with a margin. Returns
    /// the bearing (degrees) the map should be turned to so the beacon rides
    /// near the top and the user near the bottom, or `None` when not tracking
    /// or a position is missing.
    fn tracking_orientation(&mut self, ctx: &egui::Context, screen: egui::Rect) -> Option<f32> {
        let idx = self.tracking_beacon?;
        let beacons = self.beacon_positions();
        // Tracking needs both the user position and the chosen beacon. If either
        // is gone (no fix yet, beacon disconnected, index now out of range),
        // leave tracking mode and return `None` so the map unlocks instead of
        // freezing on a view it can no longer manage.
        let (Some(user), Some(&beacon)) = (self.current, beacons.get(idx)) else {
            self.tracking_beacon = None;
            return None;
        };

        // Center between the two so, once the map is turned to put the beacon
        // straight up, they sit symmetrically about the middle of the screen.
        let mid = lat_lon(
            (user.y() + beacon.y()) / 2.0,
            (user.x() + beacon.x()) / 2.0,
        );
        self.map_memory.center_at(mid);

        // Fit: scale the zoom so the on-screen separation fills the vertical
        // span left after the top/bottom margins. Mercator pixels double per
        // zoom step, so the needed change is log2(want / have). Eased toward the
        // target so entering the mode glides rather than snaps.
        let projector = Projector::new(screen, &self.map_memory, mid);
        let user_px = projector.project(user).to_pos2();
        let beacon_px = projector.project(beacon).to_pos2();
        let have = (beacon_px - user_px).length() as f64;
        let want = (screen.height() * (1.0 - 2.0 * TRACK_MARGIN_FRAC)) as f64;
        if have > 1.0 && want > 1.0 {
            let current = self.map_memory.zoom();
            let target = (current + (want / have).log2()).clamp(TRACK_ZOOM_MIN, TRACK_ZOOM_MAX);
            let dt = ctx.input(|i| i.stable_dt).clamp(0.0, 0.1) as f64;
            let alpha = 1.0 - (-dt / 0.12).exp();
            let _ = self.map_memory.set_zoom(current + (target - current) * alpha);
            if (target - current).abs() > 0.01 {
                ctx.request_repaint();
            }
        }

        Some(bearing_deg(user, beacon))
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

        // A compass thread that could not start (no rotation-vector sensor on
        // this device) drops its sender. Forgetting the handle then hides the
        // heading-up button, which is keyed off the handle existing rather than
        // off a live reading - the sensor being off is the normal state.
        let mut compass_gone = false;
        if let Some(compass) = &self.compass {
            loop {
                match compass.headings.try_recv() {
                    Ok(heading) => self.compass_heading = Some(heading),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        compass_gone = true;
                        break;
                    }
                }
            }
        }
        if compass_gone {
            self.compass = None;
        }

        while let Ok(event) = self.ble.events.try_recv() {
            match event {
                BleEvent::Status(s) => self.ble_status = s,
                BleEvent::Connected(c) => {
                    self.ble_connected = c;
                    self.connected_at = c.then(Instant::now);
                    if c {
                        // A fresh link re-reads everything below; nothing from
                        // the last session still describes the board.
                        //
                        // The intent is left alone: it says what to do when
                        // there is no link, so a board that sleeps again should
                        // still be chased if that is what was asked for.
                        self.board_settings = None;
                        self.settings_unsupported = false;
                        self.telemetry = None;
                    }
                }
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
                    self.ble_ack = Some(ack_message(&ack));
                }
                BleEvent::Telemetry(t) => self.telemetry = Some(t),
                BleEvent::Log(s) => self.board_log = Some(s),
                BleEvent::Settings(s) => {
                    // Seed the inputs from the board's own values the first
                    // time it reports them, so the boxes open on what it is
                    // actually set to. Later reports only move the controls
                    // that mirror the board (the checkboxes and the "Board:"
                    // lines), leaving anything half-typed alone.
                    if self.board_settings.is_none() {
                        self.ble_interval_text = s.notify_interval_ms.to_string();
                        if s.sleep_interval_s > 0 {
                            self.sleep_interval_text = s.sleep_interval_s.to_string();
                        }
                        // The board reports the window it resolved, so a 0 here
                        // would be a board that does not know its own effective
                        // value; keep the default rather than show it.
                        if s.adv_window_s > 0 {
                            self.adv_window_text = s.adv_window_s.to_string();
                        }
                    }
                    self.board_settings = Some(s);
                    self.settings_unsupported = false;
                }
                BleEvent::SettingsUnsupported => {
                    self.board_settings = None;
                    self.settings_unsupported = true;
                }
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
        // Heading-up may have been toggled (or dropped for want of a heading)
        // last frame; the sensor follows it here, once, for every page.
        self.sync_compass_power();

        let ctx = ui.ctx().clone();
        let screen = ctx.input(|i| i.viewport_rect());

        match self.page {
            Page::Map => self.map_page(&ctx, screen),
            Page::Data => self.data_page(&ctx, screen),
            Page::Points => self.points_page(&ctx, screen),
            Page::Status => self.status_page(&ctx, screen),
            Page::Beacon => self.beacon_page(&ctx, screen),
            Page::Settings => self.settings_page(&ctx, screen),
            Page::Radio => self.radio_page(&ctx, screen),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Reading 0 as "off" is what the interval read-outs want, and is exactly
    /// wrong for an elapsed time - the wake-mode line clamps off zero for it.
    #[test]
    fn secs_text_scales_and_calls_zero_off() {
        assert_eq!(secs_text(0), "off");
        assert_eq!(secs_text(1), "1 s");
        assert_eq!(secs_text(45), "45 s");
        assert_eq!(secs_text(60), "1 min");
        assert_eq!(secs_text(900), "15 min");
        assert_eq!(secs_text(3600), "1 h");
        assert_eq!(secs_text(43200), "12 h");
        // Not a whole number of hours, so it keeps a decimal.
        assert_eq!(secs_text(45000), "12.5 h");
    }

    /// The WIO statuses are a link failure between the ESP and the WIO-E5, not
    /// a rejected value, and have to read as something the user can act on.
    #[test]
    fn ack_message_separates_wio_faults_from_rejections() {
        let ack = |id, status| Ack {
            id,
            status,
            value_u32: None,
        };
        assert!(ack_message(&Ack {
            id: ble::CFG_ESP_SLEEP_S,
            status: packet::ACK_OK,
            value_u32: Some(300),
        })
        .unwrap()
        .contains("5 min"));

        let wio = ack_message(&ack(ble::CFG_WIO_SLEEP, ble::ACK_WIO_TIMEOUT)).unwrap_err();
        assert!(wio.contains("WIO-E5"), "{wio}");
        let bad = ack_message(&ack(ble::CFG_PWR_EN, packet::ACK_BAD_VALUE)).unwrap_err();
        assert!(bad.contains("GPS/LoRa power"), "{bad}");
        // An interval of 0 turns sleep off, and must not read as "every off".
        assert_eq!(
            ack_message(&Ack {
                id: ble::CFG_ESP_SLEEP_S,
                status: packet::ACK_OK,
                value_u32: Some(0),
            }),
            Ok("Board applied: sleep disabled".to_string())
        );
    }
}
