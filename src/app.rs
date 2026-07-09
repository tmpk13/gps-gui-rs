use std::path::PathBuf;
use std::sync::mpsc::Receiver;

use egui::emath::Rot2;
use egui::{Pos2, Shape};
use walkers::{
    lat_lon, sources::OpenStreetMap, HttpOptions, HttpTiles, Map, MapMemory, Position,
};

use crate::config::MarkerColors;
use crate::gps::GpsFix;
use crate::marker::GpsLayer;

/// Where the map looks before the first GPS fix arrives.
fn default_position() -> Position {
    lat_lon(54.333, -122.676)
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

/// Format a distance given in meters: kilometers once it is at least 1 km,
/// otherwise whole meters.
fn format_distance(m: f64) -> String {
    if m >= 1000.0 {
        format!("{:.2} km", m / 1000.0)
    } else {
        format!("{m:.0} m")
    }
}

/// Which screen is shown. The corner toggle switches between them.
#[derive(Clone, Copy, PartialEq)]
enum Page {
    /// The interactive map with the position marker and track.
    Map,
    /// A plain read-out of the current latitude and longitude.
    Data,
    /// Loading the TOML config file (marker colors).
    Settings,
}

/// HTTP tile options caching to `cache_dir` (when writable). Tiles fetched once
/// are reused from disk, so previously viewed areas keep working without a
/// network. `None` disables the cache.
fn http_options(cache_dir: Option<PathBuf>) -> HttpOptions {
    HttpOptions {
        cache: cache_dir,
        ..Default::default()
    }
}

pub struct MyApp {
    tiles: HttpTiles,
    map_memory: MapMemory,
    gps_rx: Receiver<GpsFix>,
    /// Optional device-facing compass heading stream (Android only).
    compass_rx: Option<Receiver<f32>>,
    /// Returns the current safe-area insets `[top, right, bottom, left]` in
    /// physical pixels. `None` on desktop (no system bars to avoid).
    insets: Option<Box<dyn Fn() -> [f32; 4]>>,
    current: Option<Position>,
    /// Course over ground from the GPS fix.
    heading: Option<f32>,
    /// Device-facing heading from the compass sensor.
    compass_heading: Option<f32>,
    /// When set, the map is rotated so the current heading points up.
    heading_up: bool,
    /// Rotation angle actually drawn, eased toward the live heading each frame so
    /// the map turns smoothly instead of snapping between sensor readings.
    smoothed_heading: Option<f32>,
    track: Vec<Position>,
    /// A fixed reference point a line is drawn to from the current position.
    fixed_point: Option<Position>,
    /// Which screen is currently shown.
    page: Page,
    /// Marker colors, replaced when a config file is loaded.
    marker_colors: MarkerColors,
    /// The config-file path typed on the Settings page.
    config_path: String,
    /// Result of the last load attempt: `Ok` message (green) or error (red).
    config_feedback: Option<Result<String, String>>,
}

impl MyApp {
    /// `cache_dir` is where tiles are cached to disk (`None` to disable). Desktop
    /// passes a local `.cache`; Android passes its writable data directory.
    /// `compass_rx` is the device-facing heading stream (`None` on desktop).
    /// `insets` reports the safe-area insets in physical pixels (`None` on desktop).
    pub fn new(
        ctx: egui::Context,
        gps_rx: Receiver<GpsFix>,
        cache_dir: Option<PathBuf>,
        compass_rx: Option<Receiver<f32>>,
        insets: Option<Box<dyn Fn() -> [f32; 4]>>,
    ) -> Self {
        Self {
            tiles: HttpTiles::with_options(OpenStreetMap, http_options(cache_dir), ctx),
            map_memory: MapMemory::default(),
            gps_rx,
            compass_rx,
            insets,
            current: None,
            heading: None,
            compass_heading: None,
            heading_up: false,
            smoothed_heading: None,
            track: Vec::new(),
            // Arbitrary fixed point for now: the center of the simulated loop
            // (Greenwich observatory), so the line is always in view.
            fixed_point: Some(lat_lon(51.4779, -0.0015)),
            page: Page::Map,
            marker_colors: MarkerColors::default(),
            config_path: String::new(),
            config_feedback: None,
        }
    }

    /// Load marker colors from the TOML file at `config_path`, recording a
    /// human-readable result for the Settings page to show.
    fn load_config(&mut self) {
        let path = self.config_path.trim().to_string();
        if path.is_empty() {
            self.config_feedback = Some(Err("Enter a file path.".to_string()));
            return;
        }
        self.config_feedback = Some(match MarkerColors::load(&path) {
            Ok(colors) => {
                self.marker_colors = colors;
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

    /// Device-facing compass heading if available, otherwise course over ground.
    fn effective_heading(&self) -> Option<f32> {
        self.compass_heading.or(self.heading)
    }

    /// Great-circle distance from the current position to the fixed point, in
    /// meters. `None` until both a fix and a fixed point are known.
    fn distance_to_fixed(&self) -> Option<f64> {
        match (self.current, self.fixed_point) {
            (Some(cur), Some(fixed)) => Some(haversine_m(cur, fixed)),
            _ => None,
        }
    }

    /// Pull every pending fix out of the channel, updating the current position
    /// and appending to the track.
    fn drain_gps(&mut self) {
        while let Ok(fix) = self.gps_rx.try_recv() {
            let pos = lat_lon(fix.lat, fix.lon);
            self.current = Some(pos);
            self.heading = fix.bearing;
            if self.track.last() != Some(&pos) {
                self.track.push(pos);
            }
        }

        if let Some(rx) = &self.compass_rx {
            while let Ok(heading) = rx.try_recv() {
                self.compass_heading = Some(heading);
            }
        }
    }

    fn controls(&mut self, ui: &mut egui::Ui) {
        ui.spacing_mut().button_padding = egui::vec2(15.0, 10.0);
        ui.horizontal(|ui| {

            if ui.button("Center").clicked() {
                self.map_memory.follow_my_position();
            }
            let rotate_label = if self.heading_up {
                "North"
            } else {
                "Heading"
            };
            if ui.button(rotate_label).clicked() {
                self.heading_up = !self.heading_up;
            }
            if ui.button("+").clicked() {
                let _ = self.map_memory.zoom_in();
            }
            if ui.button("-").clicked() {
                let _ = self.map_memory.zoom_out();
            }
            if ui.button("Clear").clicked() {
                self.track.clear();
            }

            ui.separator();
            match self.current {
                Some(pos) => {
                    let hdg = match self.effective_heading() {
                        Some(b) => format!("  hdg {b:.0}"),
                        None => String::new(),
                    };
                    let dist = match self.distance_to_fixed() {
                        Some(m) => format!("  dist {}", format_distance(m)),
                        None => String::new(),
                    };
                    ui.label(format!("lat {:.5}  lon {:.5}{hdg}{dist}", pos.y(), pos.x()))
                }
                None => ui.label("waiting for GPS fix..."),
            };
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
            track: self.track.clone(),
            heading: self.effective_heading(),
            fixed_point: self.fixed_point,
            colors: self.marker_colors,
        };

        // walkers sizes itself to the child's available space, so give it the
        // (possibly overscanned) map rect.
        let layer_id = ui.layer_id();
        let start = ui.ctx().graphics_mut(|g| g.entry(layer_id).next_idx());

        let android = cfg!(target_os = "android");
        // Heading-up on mobile locks the view (centered, no pan/zoom gesture);
        // the zoom buttons still work. North-up keeps normal pan.
        let locked = rotation.is_some() && android;

        // On Android the map lives in a background Area (for the rotation
        // overscan), and walkers' pinch-zoom gate does not fire there. Drive zoom
        // from the multi-touch delta ourselves, mirroring walkers' own
        // `zoom_by((delta - 1) * zoom_speed)`. Zoom is a scalar, so it is fine in
        // heading-up too (unlike pan, which stays locked while rotated).
        let zoom_delta = ui.ctx().input(|i| i.zoom_delta());
        let pinching = android && (zoom_delta - 1.0).abs() > 0.001;

        #[cfg(target_os = "android")]
        if pinching {
            let zoom = self.map_memory.zoom() + (zoom_delta as f64 - 1.0) * 2.0;
            let _ = self.map_memory.set_zoom(zoom);
        }

        // Suppress pan while pinching so the two-finger gesture zooms instead of
        // dragging (walkers normally keeps zoom and pan mutually exclusive).
        let allow_pan = !locked && !pinching;

        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(map_rect));
        let map = Map::new(Some(&mut self.tiles), &mut self.map_memory, my_position)
            .with_plugin(layer)
            // Walkers' gesture zoom works on desktop; on Android we drive it
            // manually above, so turn walkers' own gesture off there.
            .zoom_gesture(!android)
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
        self.drain_gps();

        let ctx = ui.ctx().clone();
        let screen = ctx.input(|i| i.viewport_rect());

        match self.page {
            Page::Map => self.map_page(&ctx, screen),
            Page::Data => self.data_page(&ctx, screen),
            Page::Settings => self.settings_page(&ctx, screen),
        }

        // Page toggle floats in the top-right corner, above both pages.
        self.page_toggle(&ctx, screen);
    }
}

impl MyApp {
    /// The interactive map page: full-bleed map with the floating controls.
    fn map_page(&mut self, ctx: &egui::Context, screen: egui::Rect) {
        // Rotate the map so the heading points up, when enabled and known. The
        // drawn angle eases toward the live heading each frame (shortest way
        // round the circle), so the map glides rather than stepping between
        // sensor updates. We keep requesting repaints until it settles.
        let rotation = match (self.heading_up, self.effective_heading()) {
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
                        self.controls(ui);
                    });
            });
    }

    /// The data page: a plain, large read-out of the current latitude and
    /// longitude, centered on an otherwise empty screen.
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
                            ui.add_space(screen.height() * 0.35);
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
                                    if let Some(m) = self.distance_to_fixed() {
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
                        });
                    });
            });
    }

    /// The settings page: type a path to a TOML config file and load it. The
    /// only setting for now is the marker colors; current values are shown as
    /// swatches below the loader.
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
                        color_swatch(ui, "track", self.marker_colors.track);
                        color_swatch(ui, "fixed", self.marker_colors.fixed);

                        ui.add_space(16.0);
                        ui.label(
                            egui::RichText::new(
                                "[colors]\ntrack = \"#0078ff\"\nfixed = \"#ff5028\"",
                            )
                            .monospace()
                            .size(13.0),
                        );
                    });
            });
    }

    /// Small button in the top-right corner that switches between the pages.
    fn page_toggle(&mut self, ctx: &egui::Context, screen: egui::Rect) {
        let (label, next) = match self.page {
            Page::Map => ("Data", Page::Data),
            Page::Data => ("Settings", Page::Settings),
            Page::Settings => ("Map", Page::Map),
        };
        let top = self.top_inset(ctx);
        egui::Area::new(egui::Id::new("page_toggle"))
            .order(egui::Order::Foreground)
            .fixed_pos(egui::Pos2::new(screen.right() - 8.0, top + 8.0))
            .pivot(egui::Align2::RIGHT_TOP)
            .movable(false)
            .constrain(false)
            .show(ctx, |ui| {
                if ui.button(label).clicked() {
                    self.page = next;
                }
            });
    }
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
