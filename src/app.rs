use std::path::PathBuf;
use std::sync::mpsc::Receiver;

use egui::emath::Rot2;
use egui::{Pos2, Shape};
use walkers::{
    lat_lon, sources::OpenStreetMap, HttpOptions, HttpTiles, Map, MapMemory, Position,
};

use crate::gps::GpsFix;
use crate::marker::GpsLayer;

/// Where the map looks before the first GPS fix arrives.
fn default_position() -> Position {
    lat_lon(51.4779, -0.0015)
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
        }
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
        ui.horizontal(|ui| {
            ui.heading("gps-gui-rs");
            ui.separator();

            if ui.button("Center on GPS").clicked() {
                self.map_memory.follow_my_position();
            }
            let rotate_label = if self.heading_up {
                "North up"
            } else {
                "Heading up"
            };
            if ui.button(rotate_label).clicked() {
                self.heading_up = !self.heading_up;
            }
            if ui.button("Zoom in").clicked() {
                let _ = self.map_memory.zoom_in();
            }
            if ui.button("Zoom out").clicked() {
                let _ = self.map_memory.zoom_out();
            }
            if ui.button("Clear track").clicked() {
                self.track.clear();
            }

            ui.separator();
            match self.current {
                Some(pos) => {
                    let hdg = match self.effective_heading() {
                        Some(b) => format!("  hdg {b:.0}"),
                        None => String::new(),
                    };
                    ui.label(format!("lat {:.5}  lon {:.5}{hdg}", pos.y(), pos.x()))
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
        };

        // walkers sizes itself to the child's available space, so give it the
        // (possibly overscanned) map rect.
        let layer_id = ui.layer_id();
        let start = ui.ctx().graphics_mut(|g| g.entry(layer_id).next_idx());

        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(map_rect));
        // Pinch-zoom acts in the unrotated tile space, so it fights the rotated
        // view on touch. Disable the zoom gesture while heading-up is active on
        // mobile; north-up keeps normal pinch, and the zoom buttons always work.
        let allow_pinch_zoom = !(rotation.is_some() && cfg!(target_os = "android"));
        let map = Map::new(Some(&mut self.tiles), &mut self.map_memory, my_position)
            .with_plugin(layer)
            .zoom_gesture(allow_pinch_zoom);
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
            .show(&ctx, |ui| {
                ui.set_clip_rect(map_rect);
                self.map(ui, map_rect, rotation, screen);
            });

        // Controls float on top in the foreground layer, so they keep pointer
        // priority over the (interactive) map behind them. The fill spans the
        // status-bar area; the top inset pushes the buttons clear of it.
        let top = self.top_inset(&ctx);
        egui::Area::new(egui::Id::new("controls"))
            .order(egui::Order::Foreground)
            .fixed_pos(egui::Pos2::ZERO)
            .movable(false)
            .constrain(false)
            .show(&ctx, |ui| {
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
