//! Map overlay that draws the current GPS position, heading, the track, and
//! the BLE beacon (ESP32-C3 GPS) with its optional path.

use egui::{Response, Shape, Stroke, Ui, Vec2};
use walkers::{MapMemory, Plugin, Position, Projector};

use crate::config::{MarkerColors, MarkerSizes};

/// How far the connected-beacon heartbeat ring travels, as a multiple of the
/// beacon marker's own radius. Sized off the marker so it grows with it.
const PULSE_REACH: f32 = 2.5;

/// A walkers [`Plugin`] rendering the live position marker and its trail.
///
/// It is rebuilt every frame from the app's state, so it just borrows the data
/// it needs to draw.
pub struct GpsLayer {
    pub current: Option<Position>,
    /// The phone's own path so far. Empty when it is hidden, like
    /// [`Self::beacon_track`].
    pub track: Vec<Position>,
    /// Heading in degrees clockwise from north, if known.
    pub heading: Option<f32>,
    /// Live position of the BLE GPS beacon; a line is drawn to it from the
    /// current position.
    pub beacon: Option<Position>,
    /// The beacon's path so far, drawn dashed to tell it apart from the phone's.
    /// Empty when it is hidden: the map page decides that, and hands over only
    /// what is to be drawn.
    pub beacon_track: Vec<Position>,
    /// Heartbeat phase (0..1) for the ring pulsing out of the beacon marker
    /// while the BLE link is up. `None` leaves the marker still, which is how a
    /// disconnected beacon reads.
    pub beacon_pulse: Option<f32>,
    /// Colors for the drawn markers, from the loaded config.
    pub colors: MarkerColors,
    /// Independent sizes (points) for each drawn overlay.
    pub sizes: MarkerSizes,
    /// Draw the line to the beacon dotted rather than solid.
    pub distance_dotted: bool,
}

impl Plugin for GpsLayer {
    fn run(
        self: Box<Self>,
        ui: &mut Ui,
        _response: &Response,
        projector: &Projector,
        _map_memory: &MapMemory,
    ) {
        let painter = ui.painter();
        let track_color = self.colors.track;
        let beacon_color = self.colors.fixed;
        let outline_color = self.colors.outline;
        let sizes = self.sizes;

        // The travelled track as a polyline.
        if self.track.len() >= 2 {
            let points: Vec<_> = self
                .track
                .iter()
                .map(|p| projector.project(*p).to_pos2())
                .collect();
            painter.add(Shape::line(points, Stroke::new(sizes.track, track_color)));
        }

        // The beacon's path, dashed to tell it apart from the phone track.
        if self.beacon_track.len() >= 2 {
            let points: Vec<_> = self
                .beacon_track
                .iter()
                .map(|p| projector.project(*p).to_pos2())
                .collect();
            painter.extend(Shape::dashed_line(
                &points,
                Stroke::new(sizes.track, beacon_color),
                8.0,
                6.0,
            ));
        }

        // The beacon marker itself, plus the line from the current position.
        if let Some(beacon) = self.beacon {
            let beacon_screen = projector.project(beacon).to_pos2();
            if let Some(pos) = self.current {
                let screen = projector.project(pos).to_pos2();
                let stroke = Stroke::new(sizes.distance_line, beacon_color);
                if self.distance_dotted {
                    // A dash about as long as the line is wide, with a wider gap,
                    // reads as a row of dots.
                    painter.extend(Shape::dashed_line(
                        &[screen, beacon_screen],
                        stroke,
                        sizes.distance_line,
                        sizes.distance_line * 2.0,
                    ));
                } else {
                    painter.add(Shape::line_segment([screen, beacon_screen], stroke));
                }
                // The distance label that goes on this line is painted by the map
                // page instead (see `MyApp::distance_label`), after the rotation
                // pass, so its angle can be set outright.
            }
            // Heartbeat: a ring expanding out of the marker and fading as it
            // goes, drawn under the marker so it reads as coming from it. One
            // ring per beat says the link is alive without moving the marker.
            if let Some(phase) = self.beacon_pulse {
                let radius = sizes.beacon * (1.0 + phase * PULSE_REACH);
                // Fade out over the beat, and thin the ring as it grows.
                let fade = 1.0 - phase;
                let width = sizes.beacon * 0.35 * fade;
                painter.circle_stroke(
                    beacon_screen,
                    radius,
                    Stroke::new(width, beacon_color.gamma_multiply(fade)),
                );
            }
            painter.circle_filled(beacon_screen, sizes.beacon, beacon_color);
            painter.circle_stroke(beacon_screen, sizes.beacon, Stroke::new(2.0, outline_color));
        }

        let Some(pos) = self.current else { return };
        let screen = projector.project(pos).to_pos2();

        // Heading arrow (under the dot). North is up, angle increases clockwise.
        if let Some(deg) = self.heading {
            let a = deg.to_radians();
            let dir = Vec2::new(a.sin(), -a.cos());
            let perp = Vec2::new(-dir.y, dir.x);
            let tip = screen + dir * 26.0;
            let base = screen + dir * 8.0;
            painter.add(Shape::convex_polygon(
                vec![tip, base + perp * 7.0, base - perp * 7.0],
                track_color,
                Stroke::NONE,
            ));
        }

        // The current position on top.
        painter.circle_filled(screen, sizes.marker, track_color);
        painter.circle_stroke(screen, sizes.marker, Stroke::new(2.0, outline_color));
    }
}
