//! The non-map pages: the plain Data read-out, the searchable Points list, the
//! board Status page, the Settings page, and the desktop manual-position bar.

use std::time::SystemTime;

use walkers::Position;

use midair_proto::link::{TELEM_FLAG_CFG_LOADED, TELEM_FLAG_GPS_FIX, TELEM_FLAG_SD_OK};

use crate::app::{MyApp, Page, PointFilter, RegionSelect};
use crate::ble::BleCommand;
use crate::gps::GpsFix;
use crate::points::{age_text, TrackPoint};

use super::{
    background_area, color_swatch, content_page, feedback_label, floating, status_bool,
    CORNER_MARGIN_FRAC, ERR_RED,
};

/// Format a distance given in meters: kilometers once it is at least 1 km,
/// otherwise whole meters.
fn format_distance(m: f64) -> String {
    if m >= 1000.0 {
        format!("{:.2} km", m / 1000.0)
    } else {
        format!("{m:.0} m")
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
                ui.selectable_value(&mut self.points_filter, PointFilter::Phone, "phone");
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
            let list_height = (screen.bottom() - bottom - ui.cursor().min.y - 16.0).max(60.0);
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
                                egui::RichText::new(format!("dist {}", format_distance(m)))
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
    }

    /// The settings page: load a TOML config file, and talk to the BLE
    /// beacon (status, notify interval with device ack, path toggle).
    pub(crate) fn settings_page(&mut self, ctx: &egui::Context, screen: egui::Rect) {
        let top = self.top_inset(ctx);
        content_page(ctx, "settings", screen, top, |ui| {
            ui.heading("Settings");
            ui.add_space(12.0);

            ui.label("Config file (TOML):");
            ui.horizontal(|ui| {
                let field = egui::TextEdit::singleline(&mut self.config_path)
                    .hint_text("/path/to/config.toml")
                    .desired_width((screen.width() - 120.0).clamp(120.0, 400.0));
                let resp = ui.add(field);
                let entered = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if ui.button("Load").clicked() || entered {
                    self.load_config();
                }
            });

            ui.add_space(8.0);
            feedback_label(ui, &self.config_feedback);

            ui.add_space(16.0);
            ui.label("Marker colors:");
            color_swatch(ui, "track", self.config.colors.track);
            color_swatch(ui, "beacon", self.config.colors.fixed);

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
            ui.checkbox(&mut self.show_beacon_path, "Show beacon path on map");

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label("Notify interval (ms):");
                ui.add(
                    egui::TextEdit::singleline(&mut self.ble_interval_text).desired_width(80.0),
                );
                let apply = ui.add_enabled(self.ble_connected, egui::Button::new("Apply"));
                if apply.clicked() {
                    match self.ble_interval_text.trim().parse::<u32>() {
                        Ok(ms) => {
                            let _ = self.ble.commands.send(BleCommand::SetInterval(ms));
                            self.ble_ack = None;
                            self.ble_ack_pending = true;
                        }
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
    }
}
