//! Map overlay that draws the current GPS position, heading, and the track.

use egui::{Color32, Response, Shape, Stroke, Ui, Vec2};
use walkers::{MapMemory, Plugin, Position, Projector};

const TRACK_COLOR: Color32 = Color32::from_rgb(0, 120, 255);

/// A walkers [`Plugin`] rendering the live position marker and its trail.
///
/// It is rebuilt every frame from the app's state, so it just borrows the data
/// it needs to draw.
pub struct GpsLayer {
    pub current: Option<Position>,
    pub track: Vec<Position>,
    /// Heading in degrees clockwise from north, if known.
    pub heading: Option<f32>,
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

        // The travelled track as a polyline.
        if self.track.len() >= 2 {
            let points: Vec<_> = self
                .track
                .iter()
                .map(|p| projector.project(*p).to_pos2())
                .collect();
            painter.add(Shape::line(points, Stroke::new(3.0, TRACK_COLOR)));
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
                TRACK_COLOR,
                Stroke::NONE,
            ));
        }

        // The current position on top.
        painter.circle_filled(screen, 8.0, TRACK_COLOR);
        painter.circle_stroke(screen, 8.0, Stroke::new(2.0, Color32::WHITE));
    }
}
