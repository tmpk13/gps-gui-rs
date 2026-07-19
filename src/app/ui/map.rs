//! The interactive map page: the full-bleed map, the floating controls bar,
//! marker info popups, and the offline region-download selection and progress.

use std::sync::atomic::Ordering;
use std::time::SystemTime;

use egui::emath::Rot2;
use egui::{epaint::TextShape, Pos2, Shape};
use walkers::{lat_lon, Map, Position, Projector, Tiles};

use crate::app::{MarkerKind, MyApp, RegionSelect};
use crate::marker::GpsLayer;
use crate::offline;
use crate::points::age_text;
use crate::tiles::MapLayer;

use super::{floating, icon_button, icon_button_pulse, icon_size_for, ERR_RED};

/// Where the map looks before the first GPS fix arrives.
fn default_position() -> Position {
    lat_lon(44.5, -123.0)
}

/// Refuse offline downloads bigger than this many tiles (tile-server
/// courtesy; shrink the box or lower the max zoom instead).
const MAX_REGION_TILES: u64 = 10_000;

/// Rough average size of a cached OSM tile, for the download estimate.
const TILE_SIZE_ESTIMATE_KB: u64 = 15;

/// How close (in points) a double-click must land to a marker to select it.
const MARKER_HIT_RADIUS: f32 = 40.0;

impl MyApp {
    fn controls(&mut self, ui: &mut egui::Ui, screen: egui::Rect) {
        let icon = icon_size_for(screen);
        ui.spacing_mut().button_padding = egui::vec2(icon * 0.7, icon * 0.45);
        // egui lays a horizontal row out left-to-right and can't center it in a
        // single pass: its `main_align` is ignored and the row just fills the
        // width of any centering parent. So pad the left by half the leftover
        // space, using the row width measured last frame (it stays constant once
        // the button set is fixed). `add_space` counts as an item, so drop one
        // item spacing to keep the gap even on both sides.
        let spacing = ui.spacing().item_spacing.x;
        let pad = if self.controls_width > 0.0 {
            ((ui.available_width() - self.controls_width) * 0.5 - spacing).max(0.0)
        } else {
            0.0
        };
        ui.horizontal(|ui| {
            ui.add_space(pad);
            let row = ui.horizontal(|ui| {
                // Center on the user marker if we have a fix; otherwise fall back
                // to the next available marker (the beacon). With no marker at
                // all the button pulses red and does nothing when clicked.
                let has_marker = self.current.is_some() || self.beacon.is_some();
                if icon_button_pulse(
                    ui,
                    icon,
                    egui::include_image!("../../../assets/icons/center.svg"),
                    !has_marker,
                )
                .on_hover_text("Center on position")
                .clicked()
                {
                    // Leave tracking mode: it recomputes the center every frame,
                    // so it would otherwise immediately override this recenter.
                    self.tracking_beacon = None;

                    // Center on the marker, and remember which one so the
                    // offline fallback below can pick a zoom with a cached tile.
                    let target = if let Some(pos) = self.current {
                        self.map_memory.follow_my_position();
                        Some(pos)
                    } else if let Some(pos) = self.beacon {
                        self.map_memory.center_at(pos);
                        Some(pos)
                    } else {
                        None
                    };

                    // When tiles are cached to disk, kick off the offline check:
                    // if we turn out to be offline and the current zoom has no
                    // tile for the marker, it snaps to the nearest zoom that does.
                    if let (Some(pos), Some(dir)) = (target, self.cache_dir.clone()) {
                        let current_zoom =
                            self.map_memory.zoom().round().clamp(0.0, 19.0) as u8;
                        offline::spawn_offline_zoom(
                            dir,
                            self.layer,
                            pos,
                            current_zoom,
                            self.zoom_tx.clone(),
                            ui.ctx().clone(),
                        );
                    }
                }

                // Heading-up button. Heading-up only makes sense with a direction
                // source (compass, or GPS course over ground); with none the map
                // stays north-up and the button is hidden. It is hidden while
                // tracking too, which owns the map's orientation - the track
                // button below is the way out of that mode.
                let has_direction = self.effective_heading().is_some();
                if has_direction && self.tracking_beacon.is_none() {
                    // Shows the mode the button switches TO (like the old label).
                    let (rotate_icon, rotate_hint) = if self.heading_up {
                        (
                            egui::include_image!("../../../assets/icons/north.svg"),
                            "North up",
                        )
                    } else {
                        (
                            egui::include_image!("../../../assets/icons/heading.svg"),
                            "Heading up",
                        )
                    };
                    if icon_button(ui, icon, rotate_icon)
                        .on_hover_text(rotate_hint)
                        .clicked()
                    {
                        self.heading_up = !self.heading_up;
                    }
                } else if !has_direction {
                    // No orientation available: nothing to toggle, so stay
                    // north-up and drop any stale heading-up flag.
                    self.heading_up = false;
                }

                // Tracking mode: keep the user and a beacon framed together.
                // Tapping enters the mode on the first beacon, then walks along
                // the beacon list, and the press after the last one leaves the
                // mode - this button is the only way in and out. It frames the
                // two together, so it needs BOTH a live user position and at
                // least one beacon; with either missing the button pulses red and
                // does nothing (entering with a piece missing would lock the map
                // on a view it can't frame).
                let beacons = self.beacon_positions();
                let can_track = self.current.is_some() && !beacons.is_empty();
                let tracking_hint = match self.tracking_beacon {
                    Some(i) if i + 1 < beacons.len() => "Next beacon",
                    Some(_) => "Exit tracking",
                    None => "Track beacon",
                };
                if icon_button_pulse(
                    ui,
                    icon,
                    egui::include_image!("../../../assets/icons/track.svg"),
                    !can_track,
                )
                .on_hover_text(tracking_hint)
                .clicked()
                    && can_track
                {
                    self.tracking_beacon = match self.tracking_beacon {
                        // Advance to the next beacon; past the last one the
                        // mode ends, so a single beacon is a plain on/off toggle.
                        Some(i) if i + 1 < beacons.len() => Some(i + 1),
                        Some(_) => None,
                        None => {
                            // Tracking and heading-up are mutually exclusive.
                            self.heading_up = false;
                            Some(0)
                        }
                    };
                }

                // Base-layer toggle: like the rotate button, the glyph shows the
                // layer it switches TO (a mountain for topo, the map for OSM).
                let (layer_icon, layer_hint) = match self.layer {
                    MapLayer::Standard => (
                        egui::include_image!("../../../assets/icons/topo.svg"),
                        "Topographic map",
                    ),
                    MapLayer::Topo => (
                        egui::include_image!("../../../assets/icons/map.svg"),
                        "Standard map",
                    ),
                };
                if icon_button(ui, icon, layer_icon)
                    .on_hover_text(layer_hint)
                    .clicked()
                {
                    self.layer = match self.layer {
                        MapLayer::Standard => MapLayer::Topo,
                        MapLayer::Topo => MapLayer::Standard,
                    };
                }

                // Zoom buttons are desktop-only; on mobile pinch-zoom handles
                // it, so the buttons would only crowd the small toolbar.
                if !cfg!(target_os = "android") {
                    if icon_button(ui, icon, egui::include_image!("../../../assets/icons/zoom-in.svg"))
                        .on_hover_text("Zoom in")
                        .clicked()
                    {
                        let _ = self.map_memory.zoom_in();
                    }
                    if icon_button(ui, icon, egui::include_image!("../../../assets/icons/zoom-out.svg"))
                        .on_hover_text("Zoom out")
                        .clicked()
                    {
                        let _ = self.map_memory.zoom_out();
                    }
                }
                if icon_button(ui, icon, egui::include_image!("../../../assets/icons/clear.svg"))
                    .on_hover_text("Clear tracks")
                    .clicked()
                {
                    self.track.clear();
                    self.beacon_track.clear();
                }

                // The region download is started from the Settings page, which
                // jumps back here with the box selection already active.

                // The page menu sits inline, right after the other buttons.
                self.page_menu(ui, icon);
            });
            // Remember the row's own width (the inner group, excluding the pad)
            // so the next frame can center it.
            self.controls_width = row.response.rect.width();
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
            beacon_track: if self.config.ble.show_path {
                self.beacon_track.iter().map(|t| t.pos).collect()
            } else {
                Vec::new()
            },
            show_beacon_path: self.config.ble.show_path,
            colors: self.config.colors,
            sizes: self.config.sizes,
            distance_dotted: self.config.distance.dotted,
        };

        // walkers sizes itself to the child's available space, so give it the
        // (possibly overscanned) map rect.
        let layer_id = ui.layer_id();
        let start = ui.ctx().graphics_mut(|g| g.entry(layer_id).next_idx());

        let android = cfg!(target_os = "android");
        // Tracking mode owns the center and zoom (recomputed each frame), so it
        // locks out manual pan and zoom on every platform. Heading-up on mobile
        // also locks the view. North-up keeps normal pan.
        let tracking = self.tracking_beacon.is_some();
        let locked = tracking || (rotation.is_some() && android);

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
            if pinching && !tracking {
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
            if scroll_y != 0.0 && !tracking && hover.is_some_and(|p| clip.contains(p)) {
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
        // Draw whichever base layer is selected; both share `map_memory` (so the
        // view is unchanged by the switch) and the on-disk cache.
        let tiles: &mut dyn Tiles = match self.layer {
            MapLayer::Standard => &mut self.tiles,
            MapLayer::Topo => &mut self.topo_tiles,
        };
        let map = Map::new(Some(tiles), &mut self.map_memory, my_position)
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

        // Painted last, so it is outside the rotation pass above and sits over
        // the markers.
        self.distance_label(ui, map_rect, rotation, clip);
    }

    /// Paint the beacon-distance label: the distance to the beacon, centered
    /// just above the midpoint of the user->beacon line, turning with the map.
    ///
    /// The [`GpsLayer`] plugin draws the line but not this label: text needs an
    /// angle as well as a position, and leaving that to the rotation pass in
    /// [`Self::map`] left the glyphs level. So the label is placed here, after
    /// that pass, with both set outright - positions projected exactly as the
    /// plugin projects them and turned about the same pivot, and the map's angle
    /// handed straight to the text shapes.
    fn distance_label(
        &self,
        ui: &egui::Ui,
        map_rect: egui::Rect,
        rotation: Option<Rot2>,
        clip: egui::Rect,
    ) {
        if !self.config.distance.show {
            return;
        }
        let (Some(user), Some(beacon), Some(meters)) =
            (self.current, self.beacon, self.distance_to_beacon())
        else {
            return;
        };

        // Same projector the plugin draws with: walkers builds it from the rect
        // it was given (the overscanned one when the map is rotated).
        let projector = Projector::new(map_rect, &self.map_memory, user);
        let user_px = projector.project(user).to_pos2();
        let beacon_px = projector.project(beacon).to_pos2();

        // Follow the line: turn its midpoint about the pivot the map turned
        // about, and turn the "above the line" offset with it.
        let rot = rotation.unwrap_or(Rot2::IDENTITY);
        let mid = user_px + (beacon_px - user_px) * 0.5;
        let size = self.config.sizes.distance_text;
        let pad = size * 0.4 + 4.0;
        let anchor = rotate_pos(mid, rot, clip.center()) + rot * egui::Vec2::new(0.0, -pad);

        // The label reads in the theme's text color lightened a little (the
        // outline carries the contrast, so the glyphs need not be full
        // strength), outlined in the opposite color so it stays legible over
        // either base map.
        let text_color = ui
            .visuals()
            .text_color()
            .lerp_to_gamma(egui::Color32::WHITE, 0.35);
        let outline_color = if ui.visuals().dark_mode {
            egui::Color32::BLACK
        } else {
            egui::Color32::WHITE
        };

        // Laid out once and shared by every copy below, so the outline costs
        // nine shapes but only one layout.
        let painter = ui.painter().with_clip_rect(clip);
        let galley = painter.layout_no_wrap(
            self.config.distance.units.format(meters),
            egui::FontId::proportional(size),
            text_color,
        );
        let top_left = anchor - egui::Vec2::new(galley.size().x * 0.5, galley.size().y);
        let angle = rot.angle();

        // Outline width scales with the font so it stays a hair around the
        // glyphs at any size. Diagonals are pulled in so the ring is round
        // rather than square-cornered. The offsets are applied after rotation,
        // which a symmetric ring is free to ignore.
        let w = (size * 0.1).max(1.0);
        let d = w * std::f32::consts::FRAC_1_SQRT_2;
        for off in [
            egui::Vec2::new(w, 0.0),
            egui::Vec2::new(-w, 0.0),
            egui::Vec2::new(0.0, w),
            egui::Vec2::new(0.0, -w),
            egui::Vec2::new(d, d),
            egui::Vec2::new(d, -d),
            egui::Vec2::new(-d, d),
            egui::Vec2::new(-d, -d),
        ] {
            painter.add(
                TextShape::new(top_left + off, galley.clone(), outline_color)
                    .with_override_text_color(outline_color)
                    .with_angle_and_anchor(angle, egui::Align2::CENTER_BOTTOM),
            );
        }
        painter.add(
            TextShape::new(top_left, galley, text_color)
                .with_angle_and_anchor(angle, egui::Align2::CENTER_BOTTOM),
        );
    }

    /// The interactive map page: full-bleed map with the floating controls.
    pub(crate) fn map_page(&mut self, ctx: &egui::Context, screen: egui::Rect) {
        // Box selection needs the screen to map 1:1 onto the north-up tile
        // space (the projector knows nothing about our post-rotation), so
        // heading-up rotation pauses while a region is being selected.
        let selecting = !matches!(self.select, RegionSelect::Inactive);

        // Tracking mode reframes the view between the user and the beacon and
        // returns the bearing to turn the map to (beacon up). It centers and
        // zooms as a side effect. Paused while a region box is being drawn.
        let track_bearing = if selecting {
            None
        } else {
            self.tracking_orientation(ctx, screen)
        };

        // The angle the map should be turned to: the tracking bearing wins;
        // otherwise heading-up uses the live heading. Anything else leaves the
        // map north-up.
        let target_heading = track_bearing.or(if self.heading_up && !selecting {
            self.effective_heading()
        } else {
            None
        });

        // Ease the drawn angle toward the target each frame (shortest way round
        // the circle), so the map glides rather than stepping between updates. We
        // keep requesting repaints until it settles.
        let rotation = match target_heading {
            Some(target) => {
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
            None => {
                self.smoothed_heading = None;
                None
            }
        };

        // Heading-up (without tracking, which already centered on the midpoint)
        // locks the map to the current position: it stays centered on you
        // (re-following each frame), which also makes dragging a no-op so the
        // rotated view can't be panned off. Zoom (buttons) still works.
        if track_bearing.is_none() && self.heading_up && self.current.is_some() {
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

        floating(
            ctx,
            "marker_info",
            egui::Order::Foreground,
            egui::pos2(anchor.x, anchor.y - 14.0),
            egui::Align2::CENTER_BOTTOM,
            true,
            |ui| {
                ui.label(egui::RichText::new(kind.label()).strong());
                ui.label(age);
            },
        );
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
                let mut cancel = false;
                floating(
                    ctx,
                    "select_hint",
                    egui::Order::Foreground,
                    egui::Pos2::new(screen.center().x, top + 64.0),
                    egui::Align2::CENTER_TOP,
                    false,
                    |ui| {
                        ui.horizontal(|ui| {
                            ui.label("Drag a box over the region to download");
                            if ui.button("Cancel").clicked() {
                                cancel = true;
                            }
                        });
                    },
                );
                if cancel {
                    self.select = RegionSelect::Inactive;
                }
            }
            RegionSelect::Confirm { a, b, mut max_zoom } => {
                let mut close = false;
                // Topo tiles stop at zoom 17; don't offer levels the server 404s.
                let layer_max = self.layer.max_zoom();
                floating(
                    ctx,
                    "select_confirm",
                    egui::Order::Foreground,
                    screen.center(),
                    egui::Align2::CENTER_CENTER,
                    false,
                    |ui| {
                        ui.spacing_mut().button_padding = egui::vec2(15.0, 10.0);
                        ui.label(egui::RichText::new("Download region for offline use").strong());
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            ui.label("Max zoom:");
                            if ui.add_enabled(max_zoom > 1, egui::Button::new("-")).clicked() {
                                max_zoom -= 1;
                            }
                            ui.label(format!("{max_zoom}"));
                            if ui.add_enabled(max_zoom < layer_max, egui::Button::new("+")).clicked() {
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
                                ERR_RED,
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
                                        self.layer,
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
                    },
                );
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
    pub(crate) fn download_ui(&mut self, ctx: &egui::Context, screen: egui::Rect) {
        let Some(progress) = self.download.clone() else { return };
        let bottom = self.bottom_inset(ctx);
        floating(
            ctx,
            "download_progress",
            egui::Order::Foreground,
            screen.left_bottom() + egui::vec2(8.0, -(8.0 + bottom)),
            egui::Align2::LEFT_BOTTOM,
            false,
            |ui| {
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
            },
        );
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
