//! Map overlay that draws the current GPS position, heading, and the track.

use egui::{Color32, Response, Shape, Stroke, Ui, Vec2};
use walkers::{MapMemory, Plugin, Position, Projector};

use crate::config::MarkerColors;

/// A walkers [`Plugin`] rendering the live position marker and its trail.
///
/// It is rebuilt every frame from the app's state, so it just borrows the data
/// it needs to draw.
pub struct GpsLayer {
    pub current: Option<Position>,
    pub track: Vec<Position>,
    /// Heading in degrees clockwise from north, if known.
    pub heading: Option<f32>,
    /// A fixed reference point; a line is drawn to it from the current position.
    pub fixed_point: Option<Position>,
    /// Colors for the drawn markers, from the loaded config.
    pub colors: MarkerColors,
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
        let fixed_color = self.colors.fixed;

        // The travelled track as a polyline.
        if self.track.len() >= 2 {
            let points: Vec<_> = self
                .track
                .iter()
                .map(|p| projector.project(*p).to_pos2())
                .collect();
            painter.add(Shape::line(points, Stroke::new(3.0, track_color)));
        }

        let Some(pos) = self.current else { return };
        let screen = projector.project(pos).to_pos2();

        // Line from the current position to the fixed reference point, with a
        // marker at the fixed point itself.
        if let Some(fixed) = self.fixed_point {
            let fixed_screen = projector.project(fixed).to_pos2();
            painter.add(Shape::line_segment(
                [screen, fixed_screen],
                Stroke::new(3.0, fixed_color),
            ));
            painter.circle_filled(fixed_screen, 6.0, fixed_color);
            painter.circle_stroke(fixed_screen, 6.0, Stroke::new(2.0, Color32::WHITE));
        }

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
        painter.circle_filled(screen, 8.0, track_color);
        painter.circle_stroke(screen, 8.0, Stroke::new(2.0, Color32::WHITE));
    }
}
