//! The non-map pages: the plain Data read-out, the searchable Points list, the
//! board Status page, the Settings page, and the desktop manual-position bar.

use std::time::{Duration, SystemTime};

use walkers::Position;

use midair_proto::ble;
use midair_proto::link::{TELEM_FLAG_CFG_LOADED, TELEM_FLAG_GPS_FIX, TELEM_FLAG_SD_OK};

use crate::app::{secs_text, MyApp, Page, PointFilter, RadioEdit, RegionSelect};
use crate::ble::ConfigWrite;
use crate::config::DistanceUnits;
use crate::gps::GpsFix;
use crate::points::{age_text, PointSource, TrackPoint};
use crate::radio::{EditVal, FieldType};

use super::{
    background_area, content_page, feedback_label, floating, icon_button, status_bool,
    CORNER_MARGIN_FRAC, ERR_RED, OK_GREEN,
};

/// How long after connecting the board counts as warming up. The GPS/LoRa
/// rail is off through sleep and through each wake window, and comes up only
/// once a central connects, so the WIO has to boot and the GPS has to make a
/// cold fix before there is anything to report.
const BOARD_WARMUP: Duration = Duration::from_secs(45);

/// Render the type-specific input for an unlocked radio field, bound to `val`.
/// The kind of widget follows the field's type: a draggable number, a checkbox,
/// a dropdown for an enum, or a text field.
fn radio_input(ui: &mut egui::Ui, key: &str, ty: &FieldType, val: &mut EditVal) {
    match val {
        EditVal::Int(i) => {
            ui.add(egui::DragValue::new(i));
        }
        EditVal::Float(f) => {
            ui.add(egui::DragValue::new(f));
        }
        EditVal::Bool(b) => {
            ui.checkbox(b, "");
        }
        EditVal::Str(s) => {
            if let FieldType::Enum(opts) = ty {
                egui::ComboBox::from_id_salt(("radio_enum", key))
                    .selected_text(s.clone())
                    .show_ui(ui, |ui| {
                        for opt in opts {
                            ui.selectable_value(s, opt.clone(), opt.as_str());
                        }
                    });
            } else {
                // Width in text units, not raw pixels, so it scales with the font.
                let width = ui.text_style_height(&egui::TextStyle::Body) * 12.0;
                ui.add(egui::TextEdit::singleline(s).desired_width(width));
            }
        }
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

impl MyApp {
    /// The points page: a searchable, filterable list of every recorded GPS
    /// point from both sources. Tapping a row shows it on the map.
    pub(crate) fn points_page(&mut self, ctx: &egui::Context, screen: egui::Rect) {
        let top = self.top_inset(ctx);
        let bottom = self.bottom_inset(ctx);
        content_page(ctx, "points", screen, top, |ui| {
            ui.heading("GPS points");
            ui.add_space(12.0);

            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.points_search)
                        .hint_text("search (e.g. 51.47 or central)")
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
                // The source names come from `PointSource::label`, so the
                // filter and the rows below it always read the same.
                ui.selectable_value(
                    &mut self.points_filter,
                    PointFilter::Phone,
                    PointSource::Phone.label(),
                );
                ui.selectable_value(
                    &mut self.points_filter,
                    PointFilter::Esp,
                    PointSource::Esp.label(),
                );
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
            let list_height = (screen.bottom() - bottom - ui.cursor().min.y - 16.0).max(60.0);
            let mut goto: Option<Position> = None;
            egui::ScrollArea::vertical()
                .max_height(list_height)
                .auto_shrink([false, false])
                .show_rows(ui, row_height, rows.len(), |ui, range| {
                    for p in &rows[range] {
                        // The source column is wide enough for the longest
                        // label, so the coordinates stay in line.
                        let text = format!(
                            "{:<8} {}  {:>7}",
                            p.source.label(),
                            p.coord_text(),
                            age_text(now, p.time),
                        );
                        if ui
                            .selectable_label(false, egui::RichText::new(text).monospace())
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
    }

    /// The data page: a plain, large read-out of the current latitude and
    /// longitude plus the beacon distance, centered on an otherwise empty
    /// screen.
    pub(crate) fn data_page(&mut self, ctx: &egui::Context, screen: egui::Rect) {
        background_area(ctx, "data", screen, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(screen.height() * 0.3);
                match self.current {
                    Some(pos) => {
                        ui.label(egui::RichText::new(format!("lat {:.5}", pos.y())).size(40.0));
                        ui.add_space(8.0);
                        ui.label(egui::RichText::new(format!("lon {:.5}", pos.x())).size(40.0));
                        if let Some(m) = self.distance_to_beacon() {
                            ui.add_space(24.0);
                            ui.label(
                                egui::RichText::new(format!(
                                    "dist {}",
                                    self.config.distance.units.format(m)
                                ))
                                .size(40.0),
                            );
                        }
                    }
                    None => {
                        ui.label(egui::RichText::new("waiting for GPS fix...").size(24.0));
                    }
                }

                // The beacon's own data, when it is streaming.
                if let (Some(b), Some(p)) = (self.beacon, self.beacon_packet) {
                    ui.add_space(24.0);
                    ui.label(
                        egui::RichText::new(format!("beacon {:.5} {:.5}", b.y(), b.x())).size(24.0),
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
    }

    /// Bottom-anchored bar for entering a position by hand when no live GPS
    /// source is wired up (desktop). Accepts "lat, lon" or "lat lon"; a valid
    /// entry feeds the same pipeline a real fix would and recenters the map.
    pub(crate) fn manual_gps_bar(&mut self, ctx: &egui::Context, screen: egui::Rect) {
        let bottom = self.bottom_inset(ctx);
        let margin = screen.size().min_elem() * CORNER_MARGIN_FRAC;
        floating(
            ctx,
            "manual_gps",
            egui::Order::Foreground,
            egui::Pos2::new(screen.center().x, screen.bottom() - bottom - margin),
            egui::Align2::CENTER_BOTTOM,
            false,
            |ui| {
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
                        ERR_RED,
                        "Enter latitude and longitude, e.g. 51.4779, -0.0015",
                    );
                }
            },
        );
    }

    /// The status page: board health for the esp32c6-gps board. The BLE link
    /// state comes from the connection itself; the WIO/GPS/LoRa figures come
    /// from the board's telemetry characteristic, and the last line from its
    /// log characteristic.
    pub(crate) fn status_page(&mut self, ctx: &egui::Context, screen: egui::Rect) {
        let top = self.top_inset(ctx);
        content_page(ctx, "status", screen, top, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.heading("Status");
                ui.add_space(12.0);

                // ESP32-C6 / BLE link.
                ui.strong("ESP32-C6 (BLE)");
                status_bool(ui, "Link", self.ble_connected);
                ui.label(self.ble_status.as_str());

                // The rail powering the WIO-E5 and the GPS comes up only once
                // a central connects, so an empty read-out just after
                // connecting is the board waking, not a fault.
                let warming = self
                    .connected_at
                    .is_some_and(|t| t.elapsed() < BOARD_WARMUP);
                let rail_off = self.board_settings.is_some_and(|s| !s.pwr_en);
                if rail_off {
                    ui.add_space(8.0);
                    ui.label(
                        "The GPS/LoRa power rail is switched off, so the WIO-E5 and the GPS \
                         are unpowered and report nothing. Turn it on under Settings.",
                    );
                } else if warming {
                    ui.add_space(8.0);
                    ui.label(
                        "Warming up: the rail powers on at connect, so the WIO-E5 is still \
                         booting and the GPS is working on a cold fix.",
                    );
                }

                let Some(t) = self.telemetry else {
                    ui.add_space(16.0);
                    if !warming && !rail_off {
                        ui.label(
                            "No board telemetry yet.\n\
                             Waiting for the esp32c6-gps board (an esp32c3 \
                             beacon does not report it).",
                        );
                    }
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
    }

    /// The settings page: edit the app's own TOML settings and write them back
    /// to the config file, then talk to the BLE beacon (status, notify interval
    /// with device ack).
    ///
    /// Every widget here is bound straight to the live [`crate::config::AppConfig`],
    /// so a change takes effect on the map immediately; Save is what makes it
    /// outlast the session.
    pub(crate) fn settings_page(&mut self, ctx: &egui::Context, screen: egui::Rect) {
        let top = self.top_inset(ctx);
        // The field column is the screen less room for its label and buttons.
        let field_width = (screen.width() - 200.0).clamp(120.0, 360.0);
        content_page(ctx, "settings", screen, top, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.heading("Settings");
                ui.add_space(12.0);

                ui.label("Config file (TOML):");
                ui.horizontal(|ui| {
                    let field = egui::TextEdit::singleline(&mut self.config_path)
                        .hint_text("/path/to/config.toml")
                        .desired_width(field_width);
                    let resp = ui.add(field);
                    let entered = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if ui.button("Load").clicked() || entered {
                        self.load_config();
                    }
                });

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui
                        .button("Save")
                        .on_hover_text(
                            "Write these settings to the file above, generating it if it is not there",
                        )
                        .clicked()
                    {
                        self.save_config();
                    }
                    if ui
                        .button("Reset to defaults")
                        .on_hover_text("Only in the app until you save")
                        .clicked()
                    {
                        self.reset_config();
                    }
                });

                ui.add_space(8.0);
                feedback_label(ui, &self.config_feedback);

                ui.add_space(16.0);
                ui.strong("Marker colors");
                egui::Grid::new("cfg_colors").num_columns(2).show(ui, |ui| {
                    ui.label("track");
                    ui.color_edit_button_srgba(&mut self.config.colors.track);
                    ui.end_row();
                    ui.label("beacon");
                    ui.color_edit_button_srgba(&mut self.config.colors.fixed);
                    ui.end_row();
                });

                ui.add_space(16.0);
                ui.strong("Overlay sizes (points)");
                let s = &mut self.config.sizes;
                egui::Grid::new("cfg_sizes").num_columns(2).show(ui, |ui| {
                    for (label, value) in [
                        ("marker", &mut s.marker),
                        ("beacon", &mut s.beacon),
                        ("track", &mut s.track),
                        ("distance line", &mut s.distance_line),
                        ("distance text", &mut s.distance_text),
                    ] {
                        ui.label(label);
                        // The loader rejects a size of 0 or less, so the drag stops
                        // short of one rather than writing a file that won't load.
                        ui.add(egui::DragValue::new(value).speed(0.1).range(0.5..=64.0));
                        ui.end_row();
                    }
                });

                ui.add_space(16.0);
                ui.strong("Beacon distance");
                ui.checkbox(
                    &mut self.config.distance.show,
                    "Show distance on the line to the beacon",
                );
                ui.checkbox(
                    &mut self.config.distance.dotted,
                    "Draw that line dotted rather than solid",
                );
                ui.horizontal(|ui| {
                    ui.label("Units:");
                    ui.selectable_value(
                        &mut self.config.distance.units,
                        DistanceUnits::Metric,
                        "km/m",
                    );
                    ui.selectable_value(
                        &mut self.config.distance.units,
                        DistanceUnits::Imperial,
                        "mi/ft",
                    );
                });

                ui.add_space(16.0);
                ui.strong("Track recording");
                ui.horizontal(|ui| {
                    ui.label("Minimum move between points (m):");
                    ui.add(
                        egui::DragValue::new(&mut self.config.track.min_distance)
                            .speed(0.1)
                            .range(0.0..=1000.0),
                    );
                });

                // Offline maps: start a region download. Only when tiles are cached
                // to disk; jumps to the map and begins the box selection there.
                if self.cache_dir.is_some() {
                    ui.add_space(16.0);
                    ui.separator();
                    ui.add_space(8.0);
                    ui.heading("Offline maps");
                    ui.add_space(8.0);
                    let downloading = self.download.is_some();
                    if ui
                        .add_enabled(!downloading, egui::Button::new("Download region"))
                        .on_hover_text("Pick a box on the map to cache for offline use")
                        .clicked()
                    {
                        self.page = Page::Map;
                        self.select = RegionSelect::Picking {
                            start: None,
                            current: None,
                        };
                    }
                    if downloading {
                        ui.label("A download is already in progress.");
                    }
                }

                ui.add_space(16.0);
                ui.separator();
                ui.add_space(8.0);
                ui.heading("GPS beacon (BLE)");
                ui.add_space(8.0);
                ui.label(format!("Status: {}", self.ble_status));

                ui.add_space(8.0);
                ui.checkbox(&mut self.config.ble.enabled, "Connect to the beacon");
                ui.checkbox(&mut self.config.ble.show_path, "Show beacon path on map");
                ui.horizontal(|ui| {
                    ui.label("MAC:");
                    let field = egui::TextEdit::singleline(&mut self.ble_mac_text)
                        .hint_text("empty = scan by service")
                        .desired_width(field_width);
                    if ui.add(field).changed() {
                        let mac = self.ble_mac_text.trim();
                        self.config.ble.mac = (!mac.is_empty()).then(|| mac.to_string());
                    }
                });
                ui.label(
                    egui::RichText::new("Enabled/MAC changes apply on Reconnect below.").weak(),
                );

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.label("Notify interval (ms):");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.ble_interval_text).desired_width(80.0),
                    );
                    let ready = self.ble_connected && !self.ble_ack_pending;
                    let apply = ui.add_enabled(ready, egui::Button::new("Apply"));
                    if apply.clicked() {
                        match self.ble_interval_text.trim().parse::<u32>() {
                            Ok(ms) => self.send_config(ConfigWrite::Interval(ms)),
                            Err(_) => {
                                self.ble_ack =
                                    Some(Err("Enter a whole number of milliseconds.".to_string()));
                            }
                        }
                    }
                });
                if self.ble_ack.is_none() && self.ble_ack_pending {
                    ui.label("waiting for device ack...");
                } else {
                    feedback_label(ui, &self.ble_ack);
                }

                ui.add_space(16.0);
                ui.separator();
                ui.add_space(8.0);
                ui.heading("Board power and sleep");
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(
                        "ESP32-C6 settings. The board keeps these in flash, so they outlast a \
                         power cycle.",
                    )
                    .weak(),
                );
                ui.add_space(8.0);
                self.board_power_ui(ui);

                ui.add_space(16.0);
                if ui
                    .button("Reconnect")
                    .on_hover_text("Also the way back to a stowed board")
                    .clicked()
                {
                    self.reconnect_ble();
                }
            });
        });

        // Same as the radio page: a nested Area inside the page's own Area
        // misbehaves, so the confirmation is drawn at the top level.
        self.stow_confirm_popup(ctx, screen);
    }

    /// The board's power rail, sleep switches and the two deep-sleep
    /// intervals. Every control reads the board's own settings blob rather
    /// than a local copy: the board is the authority, and it changes these by
    /// itself (clamping an interval, disarming stow when you connect). A
    /// control therefore only moves once the board reports that it moved.
    fn board_power_ui(&mut self, ui: &mut egui::Ui) {
        if let Some(secs) = self.stowed {
            ui.colored_label(OK_GREEN, format!("Stowed for {}.", secs_text(secs)));
            ui.label(
                "The board acked and dropped the link on purpose; it is asleep now. It \
                 advertises only briefly (about 15 s) on each wake, so reaching it again takes \
                 a Reconnect timed to land in one of those windows - which also disarms the \
                 stow.",
            );
            ui.add_space(6.0);
            if self.stow_persisted {
                ui.label(
                    egui::RichText::new(
                        "Auto-connect is off while it is stowed, so the app will not wake it \
                         by accident. Remembered in the config file, so a restart will not \
                         either.",
                    )
                    .weak(),
                );
            } else {
                ui.colored_label(
                    ERR_RED,
                    "There is no config file to remember this in, so a restart will \
                     auto-connect and disarm the stow. Save on this page first to keep it.",
                );
            }
            ui.add_space(8.0);
            self.wake_mode_ui(ui, Some(secs));
            return;
        }
        if !self.ble_connected {
            ui.label("Connect to the board to see and change these.");
            // A board on a wake-check interval advertises in the same brief
            // windows a stowed one does, so the same chase applies.
            ui.add_space(8.0);
            self.wake_mode_ui(ui, None);
            return;
        }
        if self.settings_unsupported {
            ui.colored_label(ERR_RED, "This board's firmware is newer than the app.");
            ui.label(
                "Its settings use a layout this build cannot decode, so what the board is set \
                 to is unknown and these controls stay hidden rather than show defaults it \
                 never reported. Update the app to change them.",
            );
            return;
        }
        let Some(s) = self.board_settings else {
            ui.label("Reading the board's settings...");
            return;
        };

        // One write at a time: while an ack is outstanding the board has not
        // yet said what it applied, and these controls show only what it has.
        let busy = self.ble_ack_pending;
        ui.add_enabled_ui(!busy, |ui| {
            let mut pwr = s.pwr_en;
            if ui
                .checkbox(&mut pwr, "GPS/LoRa power rail")
                .on_hover_text("The LDO feeding both the WIO-E5 and the GPS")
                .changed()
            {
                self.send_config(ConfigWrite::Flag {
                    id: ble::CFG_PWR_EN,
                    on: pwr,
                });
            }
            let mut wio = s.wio_sleep;
            if ui
                .checkbox(&mut wio, "WIO-E5 asleep")
                .on_hover_text("Soft sleep over the UART link, radio and GPS logging stop")
                .changed()
            {
                self.send_config(ConfigWrite::Flag {
                    id: ble::CFG_WIO_SLEEP,
                    on: wio,
                });
            }
            let mut gps = s.gps_sleep;
            if ui
                .checkbox(&mut gps, "GPS in backup mode")
                .on_hover_text("The next fix after waking is a cold one")
                .changed()
            {
                self.send_config(ConfigWrite::Flag {
                    id: ble::CFG_GPS_SLEEP,
                    on: gps,
                });
            }
        });

        ui.add_space(12.0);
        ui.strong("Wake check");
        ui.label(
            egui::RichText::new(format!(
                "While this is set the board deep-sleeps whenever nothing is connected and \
                 wakes every interval to advertise briefly. The GPS/LoRa rail stays off \
                 throughout. Clamped to {} - {}.",
                secs_text(ble::ESP_SLEEP_MIN_S),
                secs_text(ble::ESP_SLEEP_MAX_S),
            ))
            .weak(),
        );
        let width = ui.text_style_height(&egui::TextStyle::Body) * 5.0;
        ui.horizontal(|ui| {
            ui.label("Every (s):");
            ui.add(
                egui::TextEdit::singleline(&mut self.sleep_interval_text).desired_width(width),
            );
            if ui.add_enabled(!busy, egui::Button::new("Apply")).clicked() {
                match self.sleep_interval_text.trim().parse::<u32>() {
                    Ok(secs) => self.send_config(ConfigWrite::Seconds {
                        id: ble::CFG_ESP_SLEEP_S,
                        secs,
                    }),
                    Err(_) => {
                        self.ble_ack = Some(Err("Enter a whole number of seconds.".to_string()));
                    }
                }
            }
            let can_disable = !busy && s.sleep_interval_s > 0;
            if ui
                .add_enabled(can_disable, egui::Button::new("Disable"))
                .on_hover_text("Stop the board sleeping at all")
                .clicked()
            {
                self.send_config(ConfigWrite::Seconds {
                    id: ble::CFG_ESP_SLEEP_S,
                    secs: 0,
                });
            }
        });
        ui.label(match s.sleep_interval_s {
            0 => "Board: sleep disabled.".to_string(),
            secs => format!("Board: waking every {}.", secs_text(secs)),
        });

        ui.add_space(12.0);
        ui.strong("Stow");
        ui.label(
            egui::RichText::new(format!(
                "One long sleep for storage or transport. The board acks, drops the link and \
                 sleeps at once; connecting again disarms it, so it reads as off while you are \
                 linked. Clamped to {} - {}.",
                secs_text(ble::ESP_STOW_MIN_S),
                secs_text(ble::ESP_STOW_MAX_S),
            ))
            .weak(),
        );
        ui.horizontal(|ui| {
            ui.label("For (s):");
            ui.add(egui::TextEdit::singleline(&mut self.stow_interval_text).desired_width(width));
            if ui
                .add_enabled(!busy, egui::Button::new("Arm"))
                .on_hover_text("Asks for confirmation: this puts the board out of reach")
                .clicked()
            {
                match self.stow_interval_text.trim().parse::<u32>() {
                    Ok(secs) if secs > 0 => self.stow_confirm = Some(secs),
                    Ok(_) => {
                        self.ble_ack =
                            Some(Err("Zero would disarm stow, not arm it.".to_string()));
                    }
                    Err(_) => {
                        self.ble_ack = Some(Err("Enter a whole number of seconds.".to_string()));
                    }
                }
            }
        });
        if s.stow_interval_s > 0 {
            ui.label(format!(
                "Board: armed for {}.",
                secs_text(s.stow_interval_s)
            ));
        }
    }

    /// The keep-trying-to-wake toggle, with how long it has been at it.
    /// `stowed_for` is the interval the board is sleeping for when that is
    /// known, which is what sets expectations for the wait.
    fn wake_mode_ui(&mut self, ui: &mut egui::Ui, stowed_for: Option<u32>) {
        // Offering to chase a board the master switch says not to talk to
        // would only be a way to contradict it.
        if !self.config.ble.enabled {
            ui.label(
                egui::RichText::new("Connecting to the beacon is switched off above.").weak(),
            );
            return;
        }
        let mut on = self.wake_mode;
        if ui
            .checkbox(&mut on, "Keep trying to wake it")
            .on_hover_text(
                "Scan without stopping so a wake window cannot be missed. Leaves the stow \
                 alone: switching this off puts things back as they were.",
            )
            .changed()
        {
            self.set_wake_mode(on);
        }
        if !self.wake_mode {
            return;
        }

        // Clamped off zero: `secs_text` reads 0 as "off", which is right for
        // an interval and nonsense for an elapsed time.
        let waiting = self
            .wake_started
            .map_or(1, |t| t.elapsed().as_secs().max(1) as u32);
        ui.label(format!("Listening for {}.", secs_text(waiting)));
        ui.label(
            egui::RichText::new(match stowed_for {
                Some(secs) => format!(
                    "The board wakes about every {}, then advertises for around 15 s. Its \
                     clock is a free-running RC oscillator, so a wake can drift tens of \
                     minutes either way - this has to keep listening rather than time it.",
                    secs_text(secs)
                ),
                None => "The board advertises for around 15 s per wake, so this has to be \
                         listening when a window opens."
                    .to_string(),
            })
            .weak(),
        );
        // The elapsed line is the only thing on the page that moves by itself;
        // a one-second tick keeps it honest without pinning the frame rate.
        ui.ctx().request_repaint_after(Duration::from_secs(1));
    }

    /// Arming a stow is confirmed first: it disconnects immediately and leaves
    /// the board unreachable except during its brief wake windows, which for a
    /// multi-hour interval is a long time to be locked out.
    fn stow_confirm_popup(&mut self, ctx: &egui::Context, screen: egui::Rect) {
        let Some(secs) = self.stow_confirm else {
            return;
        };
        // The board clamps what it is given, so confirm against the interval
        // it will really use rather than the one that was typed.
        let applied = secs.clamp(ble::ESP_STOW_MIN_S, ble::ESP_STOW_MAX_S);
        floating(
            ctx,
            "stow_confirm",
            egui::Order::Foreground,
            screen.center(),
            egui::Align2::CENTER_CENTER,
            false,
            |ui| {
                ui.strong(format!("Stow the board for {}?", secs_text(applied)));
                ui.add_space(8.0);
                ui.label(format!(
                    "It disconnects straight away, and you will not be able to reach it for up \
                     to {}. Each wake advertises for only about 15 s, so getting back in takes \
                     a well-timed Reconnect.",
                    secs_text(applied)
                ));
                if applied != secs {
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(format!(
                            "{secs} s is outside the board's range, so it will use {applied} s."
                        ))
                        .weak(),
                    );
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Stow").clicked() {
                        self.send_config(ConfigWrite::Seconds {
                            id: ble::CFG_ESP_STOW_S,
                            secs,
                        });
                        self.stow_confirm = None;
                    }
                    if ui.button("Cancel").clicked() {
                        self.stow_confirm = None;
                    }
                });
            },
        );
    }

    /// The radio page: load the WIO-E5 RADIO.TOML, edit each setting with a
    /// type-specific input behind a per-field edit lock, and save it back -
    /// keeping the file's comments and a timestamped backup of the previous
    /// version.
    pub(crate) fn radio_page(&mut self, ctx: &egui::Context, screen: egui::Rect) {
        let top = self.top_inset(ctx);
        content_page(ctx, "radio", screen, top, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.heading("Radio config");
                ui.add_space(6.0);
                ui.label(egui::RichText::new("WIO-E5 RADIO.TOML for the esp32c6-gps board.").weak());
                ui.add_space(12.0);

                ui.label("File:");
                ui.horizontal(|ui| {
                    let field = egui::TextEdit::singleline(&mut self.radio_path)
                        .hint_text("/path/to/RADIO.toml")
                        .desired_width((screen.width() - 200.0).clamp(120.0, 360.0));
                    let resp = ui.add(field);
                    let entered =
                        resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if ui.button("Load").clicked() || entered {
                        self.load_radio();
                    }
                    let dirty = self.radio.as_ref().is_some_and(|r| r.dirty);
                    let save = egui::Button::new(if dirty { "Save *" } else { "Save" });
                    if ui.add_enabled(self.radio.is_some(), save).clicked() {
                        self.save_radio();
                    }
                });
                ui.add_space(6.0);
                feedback_label(ui, &self.radio_feedback);

                if self.radio.is_some() {
                    ui.add_space(8.0);
                    self.radio_fields_ui(ui);
                    self.radio_backups_ui(ui);
                } else {
                    ui.add_space(12.0);
                    ui.label(
                        "Load a RADIO.TOML to view and edit the radio, mesh, beacon and GPS \
                         settings.",
                    );
                    ui.add_space(8.0);
                    // With no file to load (a fresh SD card), start from the
                    // firmware defaults instead. It fills the editor only; Save
                    // is what writes the file.
                    if ui
                        .button("Generate default config")
                        .on_hover_text(
                            "Fill the editor with the firmware defaults, ready to edit and \
                             save to the file above",
                        )
                        .clicked()
                    {
                        self.default_radio();
                    }
                }
            });
        });

        // The edit-confirm popup floats above the page; a nested Area inside the
        // page's own Area misbehaves, so it is drawn here at the top level.
        self.radio_confirm_popup(ctx, screen);
    }

    /// The editable settings, grouped by their `[section]`. Each row is a
    /// read-only value with an edit lock, or - while unlocked - the typed input.
    fn radio_fields_ui(&mut self, ui: &mut egui::Ui) {
        let n = match &self.radio {
            Some(r) => r.fields.len(),
            None => return,
        };
        // A sentinel no real section equals, so the first field emits a heading.
        let mut section_shown = String::from("\u{0}");
        for i in 0..n {
            let (section, key, ty, desc) = {
                let f = &self.radio.as_ref().unwrap().fields[i];
                (
                    f.section.clone(),
                    f.key.clone(),
                    f.ty.clone(),
                    f.description.clone(),
                )
            };
            if section != section_shown {
                ui.add_space(10.0);
                ui.strong(if section.is_empty() {
                    "general"
                } else {
                    section.as_str()
                });
                ui.separator();
                section_shown = section.clone();
            }
            self.radio_field_row(ui, &section, &key, &ty, desc.as_deref());
        }
    }

    /// One field row: the key, then either the read-only value with a pencil
    /// (edit) button, or - while this field is unlocked - the typed input with a
    /// check (set) and an x (cancel). The description, if any, follows beneath.
    fn radio_field_row(
        &mut self,
        ui: &mut egui::Ui,
        section: &str,
        key: &str,
        ty: &FieldType,
        desc: Option<&str>,
    ) {
        let active = matches!(
            &self.radio_edit,
            RadioEdit::Active { section: s, key: k, .. }
                if s.as_str() == section && k.as_str() == key
        );
        // Action buttons sized to the text, so nothing is a raw pixel constant.
        let bsz = ui.text_style_height(&egui::TextStyle::Body) * 1.2;
        ui.horizontal(|ui| {
            ui.monospace(key);
            if active {
                if let RadioEdit::Active { val, .. } = &mut self.radio_edit {
                    radio_input(ui, key, ty, val);
                }
                let set = icon_button(ui, bsz, egui::include_image!("../../../assets/icons/check.svg"))
                    .on_hover_text("Set");
                if set.clicked() {
                    if let RadioEdit::Active { val, .. } = &self.radio_edit {
                        let val = val.clone();
                        if let Some(doc) = self.radio.as_mut() {
                            doc.apply(section, key, &val);
                        }
                    }
                    self.radio_edit = RadioEdit::None;
                }
                let cancel =
                    icon_button(ui, bsz, egui::include_image!("../../../assets/icons/close.svg"))
                        .on_hover_text("Cancel");
                if cancel.clicked() {
                    self.radio_edit = RadioEdit::None;
                }
            } else {
                let display = self.radio.as_ref().unwrap().display_at(section, key);
                ui.monospace(display);
                // While any field is mid-edit, lock the other pencils so only
                // one field is edited at a time.
                let busy = !matches!(self.radio_edit, RadioEdit::None);
                let edit = ui
                    .add_enabled_ui(!busy, |ui| {
                        icon_button(
                            ui,
                            bsz,
                            egui::include_image!("../../../assets/icons/edit.svg"),
                        )
                        .on_hover_text("Edit")
                    })
                    .inner;
                if edit.clicked() {
                    self.radio_edit = RadioEdit::Confirm {
                        section: section.to_string(),
                        key: key.to_string(),
                    };
                }
            }
        });
        if let Some(d) = desc {
            ui.label(egui::RichText::new(d).weak().small());
        }
        ui.add_space(6.0);
    }

    /// The floating Edit / Cancel popup shown when a field's pencil is pressed.
    /// Confirming unlocks the field for editing; cancelling clears the flow.
    fn radio_confirm_popup(&mut self, ctx: &egui::Context, screen: egui::Rect) {
        let (section, key) = match &self.radio_edit {
            RadioEdit::Confirm { section, key } => (section.clone(), key.clone()),
            _ => return,
        };
        floating(
            ctx,
            "radio_confirm",
            egui::Order::Foreground,
            screen.center(),
            egui::Align2::CENTER_CENTER,
            false,
            |ui| {
                ui.label(format!("Edit \"{key}\"?"));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Edit").clicked() {
                        let val = self
                            .radio
                            .as_ref()
                            .map(|r| r.edit_val_at(&section, &key))
                            .unwrap_or(EditVal::Str(String::new()));
                        self.radio_edit = RadioEdit::Active {
                            section: section.clone(),
                            key: key.clone(),
                            val,
                        };
                    }
                    if ui.button("Cancel").clicked() {
                        self.radio_edit = RadioEdit::None;
                    }
                });
            },
        );
    }

    /// A collapsible list of kept backups, newest first, each restorable into
    /// the editor (a restored file is unsaved until Save writes it as current).
    fn radio_backups_ui(&mut self, ui: &mut egui::Ui) {
        let backups = match &self.radio {
            Some(r) => r.backups(),
            None => return,
        };
        ui.add_space(12.0);
        ui.separator();
        egui::CollapsingHeader::new(format!("Backups ({})", backups.len()))
            .id_salt("radio_backups")
            .show(ui, |ui| {
                if backups.is_empty() {
                    ui.label("No backups yet. Saving keeps the previous version here.");
                }
                for b in &backups {
                    ui.horizontal(|ui| {
                        let name = b.file_name().and_then(|s| s.to_str()).unwrap_or("");
                        ui.monospace(name);
                        if ui.button("Restore").clicked() {
                            if let Some(doc) = self.radio.as_mut() {
                                let res = doc.restore(b);
                                self.radio_feedback = Some(match res {
                                    Ok(()) => Ok(format!("Restored {name} (unsaved - press Save)")),
                                    Err(e) => Err(e),
                                });
                            }
                            self.radio_edit = RadioEdit::None;
                        }
                    });
                }
            });
    }
}
